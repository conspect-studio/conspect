//! End-to-end orchestration: fetch → join → classify → stratified sample →
//! liveness probe → filter (nsfw + blocklist) → quirk-tagged manifest + summary.
//!
//! The networked steps are injected via [`SourceCatalog`] + [`Prober`], so this
//! whole pipeline is exercised offline with synthetic fixtures. The live run
//! wires the real HTTP-backed implementations (the `net` feature).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::iptv::classify::{Container, QuirkTag};
use crate::iptv::error::IptvError;
use crate::iptv::fetch::{ProbeOutcome, Prober, SourceCatalog};
use crate::iptv::join::join_streams_channels;
use crate::iptv::sample::{sample_sources, Plan, SelectedSource};

/// The pinned ABC News HLS endpoint. This is the canonical `WebVTT`/subtitle
/// ingest-resilience class (the bug class this whole tool exists to catch); it
/// is ALWAYS included in the soak set regardless of sampling.
pub const ABC_NEWS_URL: &str = "https://c.mjh.nz/abc-news.m3u8";

/// A local denylist: any selected source whose URL host is in `domains`, or
/// whose channel id is in `channels`, is dropped before it reaches the manifest.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Blocklist {
    /// Denied URL host substrings (matched case-insensitively against the URL).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Denied channel ids.
    #[serde(default)]
    pub channels: Vec<String>,
}

impl Blocklist {
    /// An empty blocklist (nothing denied).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a `blocklist.json` document.
    ///
    /// # Errors
    /// Returns [`IptvError::ParseBlocklist`] if the JSON is malformed.
    pub fn from_json(json: &str) -> Result<Self, IptvError> {
        serde_json::from_str(json).map_err(IptvError::ParseBlocklist)
    }

    /// Whether a selected source is denied by this blocklist.
    #[must_use]
    pub fn denies(&self, source: &SelectedSource) -> bool {
        let url_lower = source.url.to_ascii_lowercase();
        if self
            .domains
            .iter()
            .any(|d| url_lower.contains(&d.to_ascii_lowercase()))
        {
            return true;
        }
        self.channels.contains(&source.channel_id)
    }
}

/// One entry in the emitted soak manifest.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestSource {
    /// The channel id this source belongs to.
    pub channel_id: String,
    /// The (live-resolved) playable URL.
    pub url: String,
    /// The declared quality string, if any.
    pub quality: Option<String>,
    /// The `User-Agent` that must be replayed when opening this source.
    pub user_agent: Option<String>,
    /// The `Referer` that must be replayed when opening this source.
    pub referrer: Option<String>,
    /// The channel's category slugs.
    pub categories: Vec<String>,
    /// The channel's country code, if any.
    pub country: Option<String>,
    /// Always `false` (nsfw is filtered) — emitted for auditability.
    pub is_nsfw: bool,
    /// The detected delivery container.
    pub container: Container,
    /// The quirk-tag set steering the soak.
    pub quirks: Vec<QuirkTag>,
    /// The liveness probe outcome at resolve time.
    pub probe: ProbeOutcome,
}

impl ManifestSource {
    fn from_selected(s: SelectedSource, probe: ProbeOutcome) -> Self {
        Self {
            channel_id: s.channel_id,
            url: s.url,
            quality: s.quality,
            user_agent: s.user_agent,
            referrer: s.referrer,
            categories: s.categories,
            country: s.country,
            is_nsfw: s.is_nsfw,
            container: s.container,
            quirks: s.quirks,
            probe,
        }
    }
}

/// The emitted, quirk-tagged soak manifest. URLs are resolved LIVE each run and
/// are never committed to the repo (they rot + may be NSFW/DMCA-adjacent).
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    /// A note that the URLs were resolved live and must not be committed.
    pub note: String,
    /// A coarse resolve-time marker (monotonic-ish epoch seconds; best-effort).
    pub resolved_at: u64,
    /// The selected sources (live + a few deliberately-retained dead/geo).
    pub sources: Vec<ManifestSource>,
}

impl Manifest {
    /// Serialize the manifest to pretty JSON (with a trailing newline-free body).
    ///
    /// # Errors
    /// Returns [`IptvError::Serialize`] if serialization fails.
    pub fn to_pretty_json(&self) -> Result<String, IptvError> {
        serde_json::to_string_pretty(self).map_err(IptvError::Serialize)
    }

    /// Write the manifest as pretty JSON to `path`, creating parent dirs.
    ///
    /// # Errors
    /// Returns an [`IptvError`] if the directory cannot be created, the manifest
    /// cannot be serialized, or the file cannot be written.
    pub fn write_to(&self, path: &Path) -> Result<(), IptvError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IptvError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut body = self.to_pretty_json()?;
        body.push('\n');
        std::fs::write(path, body).map_err(|source| IptvError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// A human-readable summary table: per-quirk counts + live/dead totals.
    #[must_use]
    pub fn summary_table(&self) -> String {
        let mut quirk_counts: BTreeMap<QuirkTag, usize> = BTreeMap::new();
        let mut live = 0usize;
        let mut dead = 0usize;
        for s in &self.sources {
            match s.probe {
                ProbeOutcome::Live => live += 1,
                ProbeOutcome::Dead => dead += 1,
            }
            for q in &s.quirks {
                *quirk_counts.entry(*q).or_insert(0) += 1;
            }
        }
        let mut out = String::new();
        out.push_str("iptv soak source selection\n");
        out.push_str("==========================\n");
        // `write!` into a `String` is infallible; ignore the `fmt::Result`.
        let _ = writeln!(
            out,
            "total {}   LIVE {}   DEAD {}",
            self.sources.len(),
            live,
            dead
        );
        out.push_str("\nquirk distribution:\n");
        for (quirk, count) in &quirk_counts {
            let _ = writeln!(out, "  {quirk:?}: {count}");
        }
        out
    }
}

/// Build the ABC News pinned source (always present, tagged as the subtitle
/// resilience class). It is not probed against the catalog — it is the control.
fn abc_news_source(probe: ProbeOutcome) -> ManifestSource {
    let mut quirks: Vec<QuirkTag> = vec![QuirkTag::PinnedSubtitleClass];
    match Container::from_url(ABC_NEWS_URL) {
        Container::Hls => quirks.push(QuirkTag::ContainerHls),
        Container::Dash => quirks.push(QuirkTag::ContainerDash),
        Container::RawTs => quirks.push(QuirkTag::ContainerRawTs),
        Container::Other => quirks.push(QuirkTag::ContainerOther),
    }
    quirks.sort_unstable();
    ManifestSource {
        channel_id: "ABCNewsLive.pinned".to_owned(),
        url: ABC_NEWS_URL.to_owned(),
        quality: None,
        user_agent: None,
        referrer: None,
        categories: vec!["news".to_owned()],
        country: Some("US".to_owned()),
        is_nsfw: false,
        container: Container::from_url(ABC_NEWS_URL),
        quirks,
        probe,
    }
}

/// Best-effort wall-clock seconds since the Unix epoch (0 if the clock is before
/// the epoch, which cannot happen on a healthy host).
fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run the full selection pipeline.
///
/// Fetches the catalog, joins + classifies, draws a stratified over-sample,
/// probes liveness (replaying each source's required UA/referrer), then keeps
/// the first `keep_live` live sources and deliberately retains up to `keep_dead`
/// dead/geo sources (for the tile state-machine soak). NSFW + blocklisted
/// sources are filtered. The ABC News `WebVTT` class is always prepended.
///
/// # Errors
/// Returns [`IptvError`] on a catalog fetch/parse failure or if no live source
/// survives (an empty soak set is refused rather than silently emitted).
pub async fn select_sources<C, P>(
    catalog: &C,
    prober: &P,
    blocklist: &Blocklist,
    plan: &Plan,
) -> Result<Manifest, IptvError>
where
    C: SourceCatalog + Sync,
    P: Prober + Sync,
{
    let (streams, channels) = catalog.fetch().await?;
    let joined = join_streams_channels(&streams, &channels);
    let candidates = sample_sources(&joined, plan);

    let mut live: Vec<ManifestSource> = Vec::new();
    let mut dead: Vec<ManifestSource> = Vec::new();

    for source in candidates {
        if blocklist.denies(&source) {
            continue;
        }
        // (NSFW is already filtered by the sampler, but belt-and-braces.)
        if source.is_nsfw {
            continue;
        }
        let outcome = prober
            .probe(
                &source.url,
                source.user_agent.as_deref(),
                source.referrer.as_deref(),
            )
            .await;
        match outcome {
            ProbeOutcome::Live => {
                if live.len() < plan.keep_live {
                    live.push(ManifestSource::from_selected(source, outcome));
                }
            }
            ProbeOutcome::Dead => {
                if dead.len() < plan.keep_dead {
                    dead.push(ManifestSource::from_selected(source, outcome));
                }
            }
        }
        if live.len() >= plan.keep_live && dead.len() >= plan.keep_dead {
            break;
        }
    }

    if live.is_empty() {
        return Err(IptvError::NoLiveSources {
            oversample: plan.oversample,
        });
    }

    // Prepend the always-present ABC News control (probed too, so a real run
    // records its actual liveness — but it is included whether live or not).
    let abc_probe = prober.probe(ABC_NEWS_URL, None, None).await;
    let mut sources = Vec::with_capacity(1 + live.len() + dead.len());
    sources.push(abc_news_source(abc_probe));
    sources.append(&mut live);
    sources.append(&mut dead);

    Ok(Manifest {
        note: "URLs resolved live from iptv-org at run time; do NOT commit this file — \
               streams rot and may be NSFW/DMCA-adjacent. Honour per-source user_agent/referrer."
            .to_owned(),
        resolved_at: epoch_seconds(),
        sources,
    })
}
