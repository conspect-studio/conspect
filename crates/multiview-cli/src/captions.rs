//! Native in-pipeline **HLS WebVTT caption ingest** (features `ffmpeg` +
//! `overlay`).
//!
//! libav opens an HLS URL as a single program (the chosen video variant); it does
//! **not** surface the master playlist's separate `TYPE=SUBTITLES` rendition as a
//! decodable stream. So when a source's [`CaptionSelector`] resolves to an
//! HLS/`WebVTT` subtitle rendition, this module:
//!
//! 1. fetches the **master** playlist text (the source URL) via libav's I/O,
//! 2. parses it with [`multiview_input::hls::MasterPlaylist`],
//! 3. [`picks`](multiview_input::hls::MasterPlaylist::pick_subtitle) the subtitle
//!    rendition for the requested language (or the default),
//! 4. resolves the rendition's (usually relative) `URI` against the master's base
//!    directory ([`resolve_rendition_uri`]), and
//! 5. spawns an **isolated reader** thread that polls that media playlist,
//!    fetches each new `.vtt` segment and parses its cues in pure Rust
//!    ([`crate::vtt_rendition`], issue #526: no libav `webvtt` demuxer or
//!    decoder is needed, so a minimal libav build still carries captions), and
//!    publishes them into a per-source [`CaptionCueStore`].
//!
//! The reader is **best-effort**: any fetch/parse error logs and yields no
//! cues (the tile simply shows none). It only ever *writes* the lock-free cue
//! store — it can neither pace nor stall the output clock (invariant #1) nor
//! back-pressure the engine (invariant #10), exactly like the video ingest
//! threads. Every fetch is bounded in bytes and time. The off-hot-path
//! samplers read the store at the selected picture's source time.
//!
//! ## Timing (invariant #3)
//!
//! A segment's `X-TIMESTAMP-MAP` puts its cues on the MPEG-TS clock the video
//! rides (RFC 8216 §3.5; a segment without one is already on it). The reader
//! projects that instant onto the video's **normalized** source timeline
//! through the source's live video anchor (the raw container PTS and the
//! normalized PTS of one picture,
//! [`LiveAnchor`](crate::pipeline::LiveAnchor)), nearest the 33-bit wrap — the
//! timeline the selected picture's [`SourceFrameTime`] carries — so a cue
//! shows with the picture it was authored for. A source without a live anchor
//! (a local/VOD rendition) keeps the cue's time as written.
//!
//! [`SourceFrameTime`]: multiview_framestore::tile::SourceFrameTime

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use multiview_config::schema::CaptionSelector;
use multiview_core::time::MediaTime;
use multiview_ffmpeg::caption::CaptionCue;
use multiview_ffmpeg::CaptionSource;
use multiview_input::caption_store::CaptionCueStore;
use multiview_input::hls::MasterPlaylist;

/// A per-source caption cue store: `CaptionCueStore` instantiated for the unified
/// [`CaptionCue`] model. The reader thread writes it; the overlay baker samples it.
pub type CueStore = CaptionCueStore<CaptionCue>;

/// Atomic source-bound cue-store registry, updated outside the output clock.
pub type LiveCaptionStores =
    Arc<multiview_framestore::SwapCell<Arc<std::collections::HashMap<String, Arc<CueStore>>>>>;
/// Selected video picture cursor for an isolated rendition reader.
pub type DisplayCursor =
    Arc<multiview_framestore::SwapCell<Option<Arc<multiview_framestore::tile::SourceFrameTime>>>>;
/// Atomic cursor registry, modified only on a source change.
pub type DisplayCursors =
    Arc<multiview_framestore::SwapCell<Arc<std::collections::HashMap<String, DisplayCursor>>>>;

/// RT-10b: a [`multiview_overlay::CueSource`] adapter over a per-source
/// [`CueStore`], exposing its **text** cues as the routable subtitle unit a
/// [`SubtitleLayer`](multiview_overlay::SubtitleLayer) samples and can be
/// re-pointed onto.
///
/// The unified [`CaptionCue`] carries both text and bitmap (DVB-sub) shapes; this
/// adapter surfaces the **text** shape (608/708/`WebVTT`/teletext), which is what
/// `multiview-overlay`'s text [`Cue`](multiview_overlay::subtitle::Cue) models. A
/// bitmap cue at `now` yields [`None`] here (the bitmap path stays on the existing
/// per-source `sample_caption_bitmaps` sampling in the pipeline, unchanged).
/// `active_at(now)` **borrows** the active cue from the lock-free store snapshot
/// ([`CueStore::with_active_at_generation`]) and copies out only its bounded
/// projection ([`bound_cue_lines`](multiview_output::hls::bound_cue_lines): at
/// most 16 lines / 4 KiB inspected, at most 4 lines / 512 B kept), so the
/// per-tick cost on the output clock thread is O(bounded) whatever the cue
/// size a remote source sent (issue #430, invariants #1/#10). A cut cue is
/// counted exactly once in
/// [`caption_burn_stats`](crate::overlays::caption_burn_stats), keyed by the
/// store's per-cue publish sequence (unique across same-window replacements
/// and generation changes).
#[cfg(feature = "overlay")]
pub struct CueStoreSource {
    store: Arc<CueStore>,
    /// Counts a cut cue once, keyed by the store's publish sequence.
    truncated: crate::overlays::TruncationTally,
}

#[cfg(feature = "overlay")]
impl CueStoreSource {
    /// Wrap a per-source cue store as a routable text [`CueSource`](multiview_overlay::CueSource).
    #[must_use]
    pub fn new(store: Arc<CueStore>) -> Self {
        Self::with_tally(store, crate::overlays::TruncationTally::new())
    }

    /// Wrap `store`, counting cut cues through `truncated`.
    #[must_use]
    pub fn with_tally(store: Arc<CueStore>, truncated: crate::overlays::TruncationTally) -> Self {
        Self { store, truncated }
    }
}

#[cfg(feature = "overlay")]
impl Clone for CueStoreSource {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.store))
    }
}

#[cfg(feature = "overlay")]
impl multiview_overlay::CueSource for CueStoreSource {
    fn active_at(&self, now: MediaTime) -> Option<multiview_overlay::subtitle::Cue> {
        let (seq, start, end, bounded) = self
            .store
            .with_active_entry_at_generation(now, self.store.generation(), |seq, cue| match cue {
                CaptionCue::Text { start, end, text } => Some((
                    seq,
                    *start,
                    *end,
                    multiview_output::hls::bound_cue_lines(&text.lines),
                )),
                // A bitmap cue is not a text cue: the text-layer renders
                // nothing for it (the bitmap path is separate).
                _ => None,
            })
            .flatten()?;
        self.truncated.observe(seq, bounded.truncated());
        // An empty-line text cue renders nothing.
        (!bounded.is_empty()).then(|| multiview_overlay::subtitle::Cue {
            start,
            end,
            lines: bounded.into_lines(),
        })
    }
}

/// One pending subtitle re-point request: route the layer rendered into
/// `layer_id` to source `source_id`'s cues. Applied at the next sample boundary.
#[cfg(feature = "overlay")]
#[derive(Debug, Clone)]
struct SubtitleRePoint {
    layer_id: String,
    source_id: String,
}

/// The bound on pending subtitle re-points held between sample boundaries. A tiny
/// control action; a deeper backlog can only come from a pathological storm, in
/// which case the **newest** request for a layer is what the operator wants and an
/// old superseded one being shed never mis-routes. Bounded memory (safety rule §5:
/// queues drop, never grow).
#[cfg(feature = "overlay")]
const MAX_SUBTITLE_REPOINT_BACKLOG: usize = 256;

/// RT-10b: a wait-free, `Arc`-shareable handle to request subtitle re-points
/// **into the run**, without owning the (hot-loop-owned) [`SubtitleRouter`].
///
/// The router is sampled (`&mut`) on the bake consumer thread, so a re-point from
/// the control plane cannot mutate it directly. Instead the handle publishes the
/// request onto a lock-free RCU slot (the same `SwapCell` read-copy-update the cue
/// store uses) that the router **drains at the start of each `sample`** (the
/// sample-boundary apply — the subtitle analogue of the video command-drain at the
/// frame boundary). Publishing is wait-free and bounded drop-oldest, so the
/// control plane can never pace or stall the engine (invariants #1/#10). This is
/// the seam the `RouteSubtitle` command (RT-11) drives: a `RouteSubtitle` on the
/// command bus re-points the layer through this handle, and the run + tests
/// exercise the same path.
#[cfg(feature = "overlay")]
#[derive(Clone)]
pub struct SubtitleRouteHandle {
    pending: Arc<multiview_framestore::SwapCell<Arc<Vec<SubtitleRePoint>>>>,
}

#[cfg(feature = "overlay")]
impl SubtitleRouteHandle {
    /// Request that the layer rendered into `layer_id` re-point to `source_id`'s
    /// cues. Wait-free; takes effect on the router's next `sample`. Bounded
    /// drop-oldest: a backlog past [`MAX_SUBTITLE_REPOINT_BACKLOG`] sheds its
    /// oldest request (the newest binding wins).
    pub fn request_repoint(&self, layer_id: &str, source_id: &str) {
        // RCU append: clone the current pending vec, push, publish. A benign race
        // between two concurrent publishers can drop one append; re-points are rare
        // operator actions, and the loser would simply re-issue — never a mis-route.
        let current = self.pending.load();
        let mut next: Vec<SubtitleRePoint> = current.as_ref().clone();
        if next.len() >= MAX_SUBTITLE_REPOINT_BACKLOG {
            next.remove(0);
        }
        next.push(SubtitleRePoint {
            layer_id: layer_id.to_owned(),
            source_id: source_id.to_owned(),
        });
        self.pending.store(Arc::new(next));
    }
}

/// RT-10b: the per-layer subtitle crosspoint for the run.
///
/// Today's caption rendering samples one cue per **source** each output tick
/// (`source_id -> on-screen lines`) and burns it into that source's tile. This
/// router lifts that to a **per-layer** [`SubtitleLayer`](multiview_overlay::SubtitleLayer):
/// one layer per source-bound caption store, each initially pointing at its own
/// source's cues, sampled per output tick by
/// [`SubtitleLayer::sample`](multiview_overlay::SubtitleLayer::sample) (`active_at(now)`).
/// Because each layer holds an atomically-swappable
/// [`CueSource`](multiview_overlay::CueSource), a [`repoint`](Self::repoint) makes a
/// subtitle breakaway effective — the layer renders **another** source's cues on
/// the next tick (CLEAR-on-switch at the seam) — while a layer that is never
/// re-pointed samples its own source exactly as before (byte-identical steady
/// behaviour).
///
/// The full source registry is retained so a re-point can target any source's
/// cues by id. The router is sampled on the bake consumer (the same off-hot-path
/// place the old `sample_caption_stores` ran), so it never paces or stalls the
/// engine (invariants #1/#10). Re-points reach it wait-free via a
/// [`SubtitleRouteHandle`], drained at the start of each [`sample`](Self::sample).
#[cfg(feature = "overlay")]
pub struct SubtitleRouter {
    /// Live source map published off-clock by the source spawner.
    registry: LiveCaptionStores,
    /// One re-pointable subtitle layer per source-bound caption store, keyed by the
    /// source id it renders into (the tile target). Sampled (`&mut`) each tick.
    layers: std::collections::HashMap<String, multiview_overlay::SubtitleLayer>,
    /// Current cue source of each tile; breakaway may differ from its video source.
    routes: std::collections::HashMap<String, String>,
    /// The per-source cue stores, so a re-point can resolve a target source id to
    /// its [`CueSource`]. Shared by `Arc` with the readers; a lock-free read only.
    stores: std::collections::HashMap<String, Arc<CueStore>>,
    /// The wait-free pending re-point slot the [`SubtitleRouteHandle`] publishes to
    /// and `sample` drains. Shared by `Arc` with every handle clone.
    pending: Arc<multiview_framestore::SwapCell<Arc<Vec<SubtitleRePoint>>>>,
}

#[cfg(feature = "overlay")]
impl SubtitleRouter {
    /// Build a router with one layer per `(source_id, store)`, each layer initially
    /// pointing at its own source's cues (the identity routing the desugar implies).
    pub fn from_stores<I>(stores: I) -> Self
    where
        I: IntoIterator<Item = (String, Arc<CueStore>)>,
    {
        let stores: std::collections::HashMap<String, Arc<CueStore>> = stores.into_iter().collect();
        let layers = stores
            .iter()
            .map(|(id, store)| {
                let source: Arc<dyn multiview_overlay::CueSource> =
                    Arc::new(CueStoreSource::new(Arc::clone(store)));
                (id.clone(), multiview_overlay::SubtitleLayer::new(source))
            })
            .collect();
        let routes = stores.keys().map(|id| (id.clone(), id.clone())).collect();
        let registry = Arc::new(multiview_framestore::SwapCell::from_pointee(stores.clone()));
        Self {
            registry,
            layers,
            routes,
            stores,
            pending: Arc::new(multiview_framestore::SwapCell::from_pointee(Vec::new())),
        }
    }

    /// Share the live source registry with an off-clock source spawner.
    #[must_use]
    pub fn with_registry(mut self, registry: LiveCaptionStores) -> Self {
        self.registry = registry;
        self
    }

    /// A wait-free, `Arc`-shareable handle to request re-points into this router
    /// from off-thread (the control plane / `RouteSubtitle`). Every clone shares
    /// the same pending slot.
    #[must_use]
    pub fn handle(&self) -> SubtitleRouteHandle {
        SubtitleRouteHandle {
            pending: Arc::clone(&self.pending),
        }
    }

    /// Whether the router has any layers (i.e. any source-bound caption store).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Drain and apply every pending re-point published since the last sample, then
    /// sample every layer at output presentation time `now`, returning the active
    /// **text** lines per layer (`source_id -> on-screen lines`). A layer whose
    /// active cue is empty/bitmap/absent is omitted — exactly the shape
    /// `sample_caption_stores` produced, so the steady (un-re-pointed) path is
    /// unchanged. Called once per output tick on the bake consumer (off the hot
    /// loop): a non-blocking RCU drain + lock-free `active_at` reads.
    pub fn sample(&mut self, now: MediaTime) -> std::collections::HashMap<String, Vec<String>> {
        let times = self
            .stores
            .iter()
            .map(|(id, store)| {
                (
                    id.clone(),
                    multiview_framestore::tile::SourceFrameTime {
                        pts: now,
                        generation: store.generation(),
                        event_clock: None,
                    },
                )
            })
            .collect();
        self.sample_selected(&times)
    }

    /// Sample cue windows at the media time of each actually displayed picture.
    /// Missing or mismatched frame metadata hides captions rather than drifting.
    pub fn sample_selected(
        &mut self,
        times: &std::collections::HashMap<String, multiview_framestore::tile::SourceFrameTime>,
    ) -> std::collections::HashMap<String, Vec<String>> {
        // Sample-boundary apply: take the pending re-points (publish an empty vec in
        // their place) and apply each. Re-pointing is `&self` on the layer, so this
        // is sound while we hold `&mut self`. Empty (the common case) is a cheap
        // pointer load + `is_empty` check — no allocation, no work.
        let latest = self.registry.load();
        for (id, store) in latest.iter() {
            let replaced = self
                .stores
                .get(id)
                .is_none_or(|old| !Arc::ptr_eq(old, store));
            if replaced {
                self.stores.insert(id.clone(), Arc::clone(store));
                let source: Arc<dyn multiview_overlay::CueSource> =
                    Arc::new(CueStoreSource::new(Arc::clone(store)));
                self.layers
                    .insert(id.clone(), multiview_overlay::SubtitleLayer::new(source));
                self.routes.insert(id.clone(), id.clone());
            }
        }
        self.layers.retain(|id, _| latest.contains_key(id));
        self.stores.retain(|id, _| latest.contains_key(id));
        self.routes
            .retain(|id, source| latest.contains_key(id) && latest.contains_key(source));
        let pending = self.pending.swap(Arc::new(Vec::new()));
        for rp in pending.iter() {
            self.repoint(&rp.layer_id, &rp.source_id);
        }
        let mut out = std::collections::HashMap::new();
        for (id, layer) in &mut self.layers {
            let Some(source_id) = self.routes.get(id) else {
                continue;
            };
            let Some(selected) = times.get(source_id) else {
                continue;
            };
            let Some(store) = self.stores.get(source_id) else {
                continue;
            };
            if store.generation() != selected.generation {
                continue;
            }
            if let Some(cue) = layer.sample(selected.pts) {
                if !cue.lines.is_empty() {
                    out.insert(id.clone(), cue.lines);
                }
            }
        }
        out
    }

    /// Re-point the layer rendered into `layer_id` to the cues of source
    /// `source_id` — the subtitle breakaway. Takes effect on the next
    /// [`sample`](Self::sample) (CLEAR-on-switch at the seam). An unknown
    /// `layer_id` or `source_id` is a logged no-op (never a panic), so a stale
    /// route can never break the run.
    pub fn repoint(&mut self, layer_id: &str, source_id: &str) {
        let Some(layer) = self.layers.get(layer_id) else {
            tracing::warn!(
                layer = %layer_id,
                "subtitle re-point held: unknown layer id"
            );
            return;
        };
        let Some(store) = self.stores.get(source_id) else {
            tracing::warn!(
                layer = %layer_id,
                source = %source_id,
                "subtitle re-point held: unknown source id"
            );
            return;
        };
        let source: Arc<dyn multiview_overlay::CueSource> =
            Arc::new(CueStoreSource::new(Arc::clone(store)));
        layer.repoint(source);
        self.routes
            .insert(layer_id.to_owned(), source_id.to_owned());
    }
}

/// A resolved per-source caption reader plan: which rendition `m3u8` to demux and
/// the store its decoded cues are published into.
pub struct CaptionPlan {
    /// Operator HLS `user_agent`/`referrer` of the owning source, sent on every
    /// request the rendition demux makes. Values are never logged.
    pub http_headers: multiview_ffmpeg::HttpHeaders,
    /// The source id this reader serves (the tile its cues burn into).
    pub id: String,
    /// The absolute rendition media-playlist URL to demux for `WebVTT` cues.
    pub rendition_url: String,
    /// The store the decoded cues are published into (shared with the baker).
    pub store: Arc<CueStore>,
    /// Whether the rendition is a **live** (continuous, never-finishing) source.
    /// A live caption reader is supervised-reconnected on EOF/error (a transient
    /// `.vtt` 404/token-expiry never permanently kills captions); a finite VOD
    /// rendition plays out once and then stops. HLS sources are live (mirrors the
    /// video [`IngestPlan::live`] derivation). See [`caption_loop`].
    pub live: bool,
    /// Lifecycle token supplied by the video source this rendition belongs to.
    pub generation: u64,
    /// Latest source PTS of the selected video picture, published by the drive.
    pub displayed_time: Arc<
        multiview_framestore::SwapCell<Option<Arc<multiview_framestore::tile::SourceFrameTime>>>,
    >,
    /// The source's live video anchor (raw container PTS <-> normalized PTS,
    /// [`LiveAnchor`](crate::pipeline::LiveAnchor)): maps a cue's MPEG-TS time
    /// onto the timeline [`Self::displayed_time`] carries (#526). Empty ⇒ the
    /// cue's MPEG-TS time is used as is.
    pub video_anchor: crate::pipeline::LiveAnchorSlot,
}

/// Resolve a rendition's (possibly relative) `URI` against the master playlist's
/// base directory, `url`-crate-free.
///
/// Rules (RFC 8216 / RFC 3986 §5, the subset HLS uses in practice):
/// * An absolute URL (`scheme://…`) is returned unchanged.
/// * A root-relative path (`/a/b.m3u8`) replaces the master's path, keeping the
///   master's `scheme://authority`.
/// * Otherwise the relative reference is joined onto the master's **base
///   directory** (the master URL with its last path segment — the filename —
///   stripped), with any query/fragment on the master dropped.
///
/// The joined path then has its `.`/`..` segments removed (RFC 3986 §5.2.4):
/// CDNs such as `CloudFront` reject a literal `..` (HTTP 400), and ad-insertion
/// origins (AWS `MediaTailor`) emit variant URIs like `../../../manifest/…`.
/// The rendition's own query string is kept unchanged.
///
/// A master URL with no recognizable `scheme://authority` (e.g. a bare local
/// path) falls back to a simple directory join so a `file:`-less local master
/// still resolves.
#[must_use]
pub fn resolve_rendition_uri(master_url: &str, rendition_uri: &str) -> String {
    // An already-absolute rendition URI (has a scheme) is used verbatim.
    if has_scheme(rendition_uri) {
        return rendition_uri.to_owned();
    }

    // Strip any query/fragment off the master before deriving its directory.
    let master_base = master_url.split(['?', '#']).next().unwrap_or(master_url);

    // Split the master into `scheme://authority` and the path that follows.
    let (origin, path) = split_origin(master_base);

    // A root-relative rendition path replaces the master's path entirely.
    if rendition_uri.starts_with('/') {
        let normalised = remove_dot_segments(rendition_uri);
        return match origin {
            Some(origin) => format!("{origin}{normalised}"),
            // No origin (a bare local master): treat as a filesystem-absolute path.
            None => normalised,
        };
    }

    // Directory of the master path: everything up to and including the last '/'.
    let dir = match path.rfind('/') {
        Some(idx) => path.get(..=idx).unwrap_or(""),
        None => "",
    };
    let joined = remove_dot_segments(&format!("{dir}{rendition_uri}"));
    match origin {
        Some(origin) => format!("{origin}{joined}"),
        None => joined,
    }
}

/// Whether `s` begins with a URL scheme (`scheme://`), so it is already absolute.
fn has_scheme(s: &str) -> bool {
    match s.find("://") {
        Some(idx) => {
            let scheme = s.get(..idx).unwrap_or("");
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        }
        None => false,
    }
}

/// Split a URL into its `scheme://authority` origin and the path that follows.
///
/// Returns `(Some(origin), path)` for an `scheme://authority/path` URL (origin
/// has **no** trailing slash, path **starts** with the leading slash, or is empty
/// when the URL is just an origin); `(None, whole)` when there is no
/// `scheme://authority` (a bare path).
fn split_origin(url: &str) -> (Option<&str>, &str) {
    let Some(scheme_end) = url.find("://") else {
        return (None, url);
    };
    let after = scheme_end.saturating_add(3);
    let authority_start = url.get(after..).unwrap_or("");
    // The path begins at the first '/' after the authority.
    match authority_start.find('/') {
        Some(rel) => {
            let split = after.saturating_add(rel);
            (url.get(..split), url.get(split..).unwrap_or(""))
        }
        // Origin only, no path (e.g. `https://host`).
        None => (Some(url), ""),
    }
}

/// Whether a [`multiview_config::Source`]'s selector resolves to the **native
/// in-container DVB-sub (bitmap) caption** path: a `Ts`/`File` source with an
/// `Auto` or `Track` selector. The DVB-sub stream lives in the SAME container as
/// the video, so this path decodes it on the source's own ingest thread (no
/// second demux) and publishes bitmap cues into the per-source store.
///
/// `TeletextPage` is explicitly **not** this path (it is the teletext decoder),
/// and `Off`/`EmbeddedCc`/`Sidecar` are other paths. HLS sources take the
/// separate WebVTT-rendition path ([`webvtt_language`]).
#[must_use]
pub fn dvbsub_selector(kind: &multiview_config::SourceKind, selector: &CaptionSelector) -> bool {
    use multiview_config::SourceKind;
    // Only an in-container TS/file source carries a muxed DVB-sub stream the
    // video-ingest thread can decode as a sibling of the video packets.
    if !matches!(kind, SourceKind::Ts { .. } | SourceKind::File { .. }) {
        return false;
    }
    matches!(
        selector,
        CaptionSelector::Auto | CaptionSelector::Track { .. }
    )
}

/// Whether a source's selector resolves to the **in-container subtitle** path:
/// a `Ts`/`File` source with an `Auto`, `Track`, or `TeletextPage` selector. The
/// subtitle stream lives in the SAME container as the video (a TS DVB-sub /
/// teletext PID, an MP4 `mov_text` track, a TS/MKV `ass`/`subrip` stream), so it
/// is decoded on the source's own ingest thread (no second demux) and publishes
/// its cues (bitmap or text) into the per-source store.
///
/// The concrete decoder is chosen from `(selector, actual stream codec)` at
/// resolve time by [`incontainer_caption_source`] — `Auto`/`Track` map by codec
/// (`dvbsub`→bitmap, `ass`/`subrip`/`mov_text`→text, `dvb_teletext`→teletext),
/// while `TeletextPage` pins the page and requires a teletext stream.
/// `EmbeddedCc`/`Off`/`Sidecar` are other paths; HLS takes the WebVTT-rendition
/// path ([`webvtt_language`]).
#[must_use]
pub fn incontainer_selector_active(
    kind: &multiview_config::SourceKind,
    selector: &CaptionSelector,
) -> bool {
    use multiview_config::SourceKind;
    if !matches!(kind, SourceKind::Ts { .. } | SourceKind::File { .. }) {
        return false;
    }
    matches!(
        selector,
        CaptionSelector::Auto
            | CaptionSelector::Track { .. }
            | CaptionSelector::TeletextPage { .. }
    )
}

/// Resolve the [`CaptionSource`] for an in-container subtitle stream from the
/// source's `selector` and the actual subtitle stream's libav `codec_name`, or
/// `None` when this path does not decode that combination (the tile then shows no
/// caption rather than building a wrong/empty decoder).
///
/// * `Auto`/`Track` map purely by **codec**: `dvbsub`→DVB-sub bitmap,
///   `ass`/`ssa`→ASS, `subrip`/`srt`/`text`→`SubRip`, `mov_text`/`tx3g`→MP4 timed
///   text, `dvb_teletext`→teletext (decoder default page).
/// * `TeletextPage { page }` pins the teletext **page** but only when the stream
///   really is a teletext (`dvb_teletext`) stream; a `TeletextPage` selector over
///   a non-teletext subtitle stream resolves to `None` (honest: that page is not
///   present here).
#[must_use]
pub fn incontainer_caption_source(
    selector: &CaptionSelector,
    codec_name: &str,
) -> Option<CaptionSource> {
    match selector {
        CaptionSelector::TeletextPage { page } => {
            if matches!(codec_name, "dvb_teletext" | "teletext") {
                Some(CaptionSource::Teletext { page: Some(*page) })
            } else {
                None
            }
        }
        CaptionSelector::Auto | CaptionSelector::Track { .. } => match codec_name {
            "dvbsub" | "dvb_subtitle" => Some(CaptionSource::DvbSubtitle),
            "ass" | "ssa" => Some(CaptionSource::Ass),
            "subrip" | "srt" | "text" => Some(CaptionSource::SubRip),
            "mov_text" | "tx3g" => Some(CaptionSource::MovText),
            "dvb_teletext" | "teletext" => Some(CaptionSource::Teletext { page: None }),
            _ => None,
        },
        // Off / embedded-cc / sidecar are not the in-container subtitle path.
        _ => None,
    }
}

/// Resolve a [`CaptionSelector::EmbeddedCc`]'s `field` string to the
/// [`CcChannel`](multiview_ffmpeg::CcChannel) the `cc_dec` embedded-CC decoder
/// should surface, for a source whose video stream may carry A53 captions.
///
/// Embedded CEA-608/708 captions are side data on the **video** frames (not a
/// separate stream — captions.md §2/§4), so this path applies to **any** source
/// whose video the runtime decodes (TS/File/HLS/RTSP/RTMP/SRT). It returns
/// `Some(channel)` only for an `embedded_cc` selector naming a **608 field**
/// (`cc1`..`cc4`); a `708 service:N` field is a real but **undecodable-to-text**
/// form for the linked `cc_dec` (it discards DTVCC service blocks), so it logs and
/// returns [`None`] rather than wiring a silently cue-less decoder (the same honest
/// refusal [`CaptionDecoder::open`](multiview_ffmpeg::CaptionDecoder) enforces).
/// An unrecognised field also logs and returns [`None`]. Every other selector
/// (`Auto`/`Off`/`TeletextPage`/`Track`/`Sidecar`) is not this path.
///
/// `auto` does **not** route here: embedded-CC presence is only known once A53
/// side data actually flows, so it is selected explicitly via `embedded_cc`
/// (captions.md §6), keeping the DVB-sub/teletext/WebVTT `auto` paths unchanged.
#[must_use]
pub fn embedded_cc_channel(
    kind: &multiview_config::SourceKind,
    selector: &CaptionSelector,
) -> Option<multiview_ffmpeg::CcChannel> {
    use multiview_config::SourceKind;
    // A53 captions ride on the decoded video; only kinds whose video the runtime
    // decodes via libav carry them (synthetic/NDI sources have no A53 stream).
    let video_decoded = matches!(
        kind,
        SourceKind::Ts { .. }
            | SourceKind::File { .. }
            | SourceKind::Hls { .. }
            | SourceKind::Rtsp { .. }
            | SourceKind::Rtmp { .. }
            | SourceKind::Srt { .. }
    );
    if !video_decoded {
        return None;
    }
    let CaptionSelector::EmbeddedCc { field } = selector else {
        return None;
    };
    parse_cc_field(field)
}

/// Parse an embedded-CC `field` string into a [`CcChannel`](multiview_ffmpeg::CcChannel).
///
/// Accepts `cc1`..`cc4` (case-insensitive) for the four CEA-608 fields. A
/// `service:N` / `708:N` token names a CEA-708 service — a real form the linked
/// `cc_dec` cannot decode to text — and is **refused** here (logged, [`None`]),
/// mirroring the decoder's own honest refusal. Any other token logs and yields
/// [`None`] (the tile simply shows no caption — never a panic, never a stall).
fn parse_cc_field(field: &str) -> Option<multiview_ffmpeg::CcChannel> {
    use multiview_ffmpeg::CcChannel;
    match field.trim().to_ascii_lowercase().as_str() {
        "cc1" => Some(CcChannel::Cc1),
        "cc2" => Some(CcChannel::Cc2),
        "cc3" => Some(CcChannel::Cc3),
        "cc4" => Some(CcChannel::Cc4),
        other => {
            // A 708 service token is a known-undecodable form; anything else is
            // unrecognised. Either way: log honestly and decline (no silent
            // cue-less decoder), exactly like the decoder's construction guard.
            if other.starts_with("service:") || other.starts_with("708:") || other == "708" {
                tracing::warn!(
                    field = %field,
                    "embedded-CC 708 service text is not decodable by the linked cc_dec; \
                     falling back to no embedded captions (use a 608 field cc1..cc4, teletext, \
                     or a sidecar)"
                );
            } else {
                tracing::warn!(field = %field, "unrecognised embedded-CC field; no embedded captions");
            }
            None
        }
    }
}

/// Publish each decoded cue (text **or** bitmap) into the store using its own
/// `[start, end)` window. The shared, shape-agnostic publish behind both the
/// DVB-sub bitmap route and the embedded-CC / in-container TEXT routes: each is
/// decoded inside the source's already-PTS-paced video ingest loop, so the cue
/// is published at the media instant its packet/frame arrives (no separate
/// wall-clock pacing needed). The store is the lock-free hand-off the
/// off-hot-path baker samples per tick (#1/#10).
pub fn publish_window_cues(store: &CueStore, cues: Vec<CaptionCue>) {
    publish_window_cues_for_generation(store, store.generation(), cues);
}

/// Publish only when a cue's decoder still owns the selected source lifecycle.
pub fn publish_window_cues_for_generation(
    store: &CueStore,
    generation: u64,
    cues: Vec<CaptionCue>,
) {
    for cue in cues {
        let (start, end) = (cue.start(), cue.end());
        store.publish_for_generation(generation, start, end, cue);
    }
}

/// Whether a [`multiview_config::Source`] should attempt native HLS `WebVTT` caption
/// ingest for the given selector, and the language to prefer.
///
/// Returns `Some(language)` (the language may itself be `None` for "default")
/// when the selector resolves to the HLS/`WebVTT` rendition path:
/// * [`CaptionSelector::Auto`] on any HLS source,
/// * [`CaptionSelector::Track`] (treated as a language tag) on any HLS source.
///
/// Other selectors (`Off`, `TeletextPage`, `EmbeddedCc`, `Sidecar`) are **not**
/// this module's path and return `None` (Phase 2/3 or the sidecar path).
#[must_use]
pub fn webvtt_language(
    kind: &multiview_config::SourceKind,
    selector: &CaptionSelector,
) -> Option<Option<String>> {
    use multiview_config::SourceKind;
    // Only an HLS source carries a master playlist with a SUBTITLES rendition.
    if !matches!(kind, SourceKind::Hls { .. }) {
        return None;
    }
    match selector {
        CaptionSelector::Auto => Some(None),
        CaptionSelector::Track { id } => Some(Some(id.clone())),
        // Off / teletext / embedded-cc / sidecar are not the HLS-rendition path.
        _ => None,
    }
}

/// Build a [`CaptionPlan`] for `source` if its selector resolves to an HLS
/// `WebVTT` rendition: fetch + parse the master, pick the subtitle rendition for the
/// requested language, and resolve its rendition URL.
///
/// Best-effort: a fetch/parse failure or a master with no usable subtitle
/// rendition logs and returns `None` (the source simply shows no captions — it
/// must never fail the pipeline build of a live source).
#[must_use]
pub fn caption_plan_for(source: &multiview_config::Source) -> Option<CaptionPlan> {
    let headers = source_http_headers(&source.kind);
    if headers.is_empty() {
        return caption_plan_for_with(source, &LibavFetcher);
    }
    tracing::debug!(source = %source.id, "caption master fetch carries operator request headers");
    caption_plan_for_with(
        source,
        &crate::pipeline::HeaderPlaylistFetcher(crate::hls_fetch::RequestHeaders::new(
            headers.user_agent(),
            headers.referer(),
        )),
    )
}

/// The operator HLS request headers a source kind carries (empty otherwise).
fn source_http_headers(kind: &multiview_config::SourceKind) -> multiview_ffmpeg::HttpHeaders {
    match kind {
        multiview_config::SourceKind::Hls {
            user_agent,
            referrer,
            ..
        } => multiview_ffmpeg::HttpHeaders::new(user_agent.as_deref(), referrer.as_deref()),
        _ => multiview_ffmpeg::HttpHeaders::default(),
    }
}

/// [`caption_plan_for`] with an injectable [`PlaylistFetcher`] — the
/// fetch→parse→pick→resolve seam, exercised offline in tests without the network.
#[must_use]
pub fn caption_plan_for_with(
    source: &multiview_config::Source,
    fetcher: &dyn PlaylistFetcher,
) -> Option<CaptionPlan> {
    let selector = source.captions.as_ref()?;
    let language = webvtt_language(&source.kind, selector)?;
    let master_url = hls_url(&source.kind)?;
    let mut plan = resolve_caption_plan(&source.id, master_url, language.as_deref(), fetcher)?;
    plan.http_headers = source_http_headers(&source.kind);
    Some(plan)
}

/// Resolve EVERY source's HLS `WebVTT` caption plan **concurrently**, off the
/// serial build path (#48). Each source's master-playlist fetch is independent
/// and network-bound, so running them on one scoped thread apiece overlaps the
/// I/O: the build waits roughly the slowest single fetch, not the sum of all of
/// them (the old loop fetched one source after another, each up to the 30s
/// budget). Non-captioned / non-HLS sources resolve to nothing and are dropped.
/// Best-effort throughout — a source whose master cannot be fetched/parsed simply
/// yields no plan and never fails the build (invariants #1/#10).
#[must_use]
pub fn resolve_caption_plans(sources: &[multiview_config::Source]) -> Vec<CaptionPlan> {
    par_filter_map(sources, caption_plan_for)
}

/// [`resolve_caption_plans`] with an injectable, thread-shareable fetcher — the
/// concurrent fetch seam, exercised offline in tests with canned bytes (no
/// network, no FFI).
#[must_use]
#[cfg(test)]
fn resolve_caption_plans_with(
    sources: &[multiview_config::Source],
    fetcher: &(dyn PlaylistFetcher + Sync),
) -> Vec<CaptionPlan> {
    par_filter_map(sources, |source| caption_plan_for_with(source, fetcher))
}

/// Map every item in `items` through `f` on its OWN scoped thread (all at once,
/// no serial blocking and no concurrency cap), collecting the `Some` results in
/// input order (the threads run concurrently but are joined in spawn order).
/// `f` must be `Sync` (it is shared across the threads) and its
/// output `Send`; empty input does no work. This is the concurrency primitive
/// behind [`resolve_caption_plans`]: N independent, blocking, network-bound
/// fetches overlap instead of serialising. A panicking closure is logged and its
/// item dropped (best-effort), rather than poisoning the whole resolve.
fn par_filter_map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> Option<R> + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let f = &f;
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .iter()
            .map(|item| scope.spawn(move || f(item)))
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| {
                if let Ok(result) = handle.join() {
                    result
                } else {
                    tracing::warn!(
                        "a caption-plan resolver thread panicked; that source shows no captions"
                    );
                    None
                }
            })
            .collect()
    })
}

/// Fetch + parse the master at `master_url`, pick the `language` subtitle
/// rendition, and resolve its URL into a [`CaptionPlan`].
///
/// Best-effort: a fetch/parse failure or a master with no usable subtitle
/// rendition logs and returns `None` (the source simply shows no captions — it
/// must never fail the pipeline build of a live source). The injected `fetcher`
/// is responsible for being robust (the [`LibavFetcher`] retries transient
/// network errors); a *persistent* failure still yields `None`.
fn resolve_caption_plan(
    id: &str,
    master_url: &str,
    language: Option<&str>,
    fetcher: &dyn PlaylistFetcher,
) -> Option<CaptionPlan> {
    let playlist = match fetcher.fetch(master_url) {
        Ok(playlist) => playlist,
        Err(reason) => {
            tracing::warn!(source = %id, %reason, "could not fetch HLS master for captions");
            return None;
        }
    };
    let master = match MasterPlaylist::parse(&playlist.body) {
        Ok(master) => master,
        Err(err) => {
            tracing::warn!(source = %id, error = %err, "HLS master parse failed; no captions");
            return None;
        }
    };
    let rendition = master.pick_subtitle(language)?;
    let uri = rendition.uri.as_deref()?;
    // Resolve the (usually relative) rendition URI against the master's EFFECTIVE
    // (post-redirect) URL — a redirecting/CDN-fronted master (c.mjh.nz -> Akamai)
    // serves relative children that only resolve under the final base, not the
    // requested one (RFC 3986 §5 / RFC 8216). For a non-redirecting fetch the
    // effective URL equals the requested URL, so this is unchanged.
    let rendition_url = resolve_rendition_uri(&playlist.url, uri);
    // #526 review: a network master may only name a network rendition. A
    // remote playlist must never steer the reader onto a local file.
    if is_network_url(&playlist.url) && !is_network_url(&rendition_url) {
        tracing::warn!(
            source = %id,
            "HLS master names a non-network subtitle rendition; no captions"
        );
        return None;
    }
    tracing::info!(
        source = %id,
        language = ?rendition.language,
        %rendition_url,
        "native HLS WebVTT caption rendition resolved"
    );
    Some(CaptionPlan {
        http_headers: multiview_ffmpeg::HttpHeaders::default(),
        id: id.to_owned(),
        rendition_url,
        store: Arc::new(CueStore::new()),
        // This planner is only reached for an HLS source (via `webvtt_language` →
        // `hls_url`); HLS is a live source (mirrors the video `IngestPlan` deriving
        // `live = true` for `SourceKind::Hls`), so its caption rendition is
        // supervised-reconnected.
        live: true,
        generation: 1,
        displayed_time: Arc::new(multiview_framestore::SwapCell::empty()),
        // Bound to the video producer's own slot when the pipeline wires the
        // plan (startup and live spawn); empty until then.
        video_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
    })
}

/// The HLS master URL for a source kind, if it is an HLS source.
fn hls_url(kind: &multiview_config::SourceKind) -> Option<&str> {
    match kind {
        multiview_config::SourceKind::Hls { url, .. } => Some(url.as_str()),
        _ => None,
    }
}

/// A fetched playlist: its body plus the **effective** URL it was actually read
/// from — i.e. the final URL **after any HTTP redirects**.
///
/// Relative child URIs in an HLS playlist resolve against the playlist's effective
/// URI, not the URL originally requested (RFC 3986 §5 / RFC 8216). A redirecting
/// or CDN-fronted master (e.g. `c.mjh.nz/abc-news.m3u8` → a signed Akamai master
/// with relative variant/rendition URIs) only resolves correctly when its children
/// are joined onto this post-redirect base. When the fetch did not redirect (or the
/// protocol exposes no final-location, e.g. `file:`), `url` equals the requested
/// URL and resolution is unchanged.
#[derive(Debug, Clone)]
pub struct FetchedPlaylist {
    /// The effective (final, post-redirect) URL the body was read from. Relative
    /// child URIs resolve against this base.
    pub url: String,
    /// The playlist body text.
    pub body: String,
}

/// Fetches a (small) playlist URL into text. Injected into the caption planner so
/// the fetch→parse→pick seam can be exercised offline with canned bytes.
pub trait PlaylistFetcher {
    /// Fetch the text of `url`, returning the body together with the **effective**
    /// (post-redirect) URL it was read from (see [`FetchedPlaylist`]).
    ///
    /// # Errors
    ///
    /// Returns `Err(reason)` describing the failure (a disallowed scheme, a
    /// network/I/O error, an oversize body, …).
    fn fetch(&self, url: &str) -> Result<FetchedPlaylist, String>;
}

/// URL schemes the caption fetch may reach — handed to libav as its
/// `protocol_whitelist` and validated in `multiview-ffmpeg` *before* opening, so a
/// stray `file:`/`concat:` master URL can never be read.
const ALLOWED_PROTOCOLS: &str = "http,https,tls,tcp";
/// Bounded master-fetch attempts. The old single-shot `curl` had no retry, so one
/// transient blip silently disabled captions for the entire run.
const FETCH_ATTEMPTS: u32 = 3;
/// Backoff between fetch attempts.
const FETCH_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);
/// Total wall-clock budget across all attempts for one source's master fetch. A
/// *fast* transient failure retries up to [`FETCH_ATTEMPTS`]; a *hung* connection
/// (eating ~one `rw_timeout`) stops after the first attempt rather than stacking
/// ~3× timeouts onto the serial pipeline build (per the #42 review).
const FETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// The production [`PlaylistFetcher`]: reads an `http(s)` URL **in-process** over
/// libav I/O ([`multiview_ffmpeg::fetch_url_text`] — bounded, scheme-whitelisted,
/// timed, and retried), or a bare local path from disk. No `curl` shell-out and
/// no extra HTTP dependency (the libav stack is already linked).
pub struct LibavFetcher;

impl PlaylistFetcher for LibavFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedPlaylist, String> {
        if has_scheme(url) {
            // `fetch_url_text` follows HTTP redirects and surfaces the EFFECTIVE
            // (final, post-redirect) URL alongside the body — relative child URIs
            // resolve against that base, not the requested URL (the ABC/Akamai
            // footgun where a `c.mjh.nz` master redirects to a signed Akamai master
            // with relative variant/rendition URIs).
            fetch_with_retry(
                || {
                    multiview_ffmpeg::fetch_url_text(url, MAX_PLAYLIST_BYTES, ALLOWED_PROTOCOLS)
                        .map(|fetched| FetchedPlaylist {
                            url: fetched.url,
                            body: fetched.body,
                        })
                        .map_err(|e| e.to_string())
                },
                FETCH_ATTEMPTS,
                FETCH_BACKOFF,
                FETCH_BUDGET,
            )
        } else {
            // A bare local path (no scheme): read it from disk. A local file never
            // redirects, so the effective URL is the requested path verbatim.
            std::fs::read_to_string(url)
                .map(|body| FetchedPlaylist {
                    url: url.to_owned(),
                    body,
                })
                .map_err(|e| e.to_string())
        }
    }
}

/// Run `attempt` up to `attempts` times, sleeping `backoff` between tries, and
/// return the first success or the last error. A bounded retry turns a single
/// transient network blip from "captions silently disabled for the whole run"
/// into a recoverable hiccup — the actual defect behind the empty caption band.
fn fetch_with_retry<T>(
    mut attempt: impl FnMut() -> Result<T, String>,
    attempts: u32,
    backoff: std::time::Duration,
    budget: std::time::Duration,
) -> Result<T, String> {
    let n = attempts.max(1);
    let start = std::time::Instant::now();
    let mut last = String::from("no fetch attempt was made");
    for i in 1..=n {
        match attempt() {
            Ok(text) => return Ok(text),
            Err(reason) => {
                tracing::debug!(attempt = i, max = n, %reason, "caption master fetch attempt failed");
                last = reason;
                if i >= n {
                    break;
                }
                // Stop early if the total wall-clock budget is spent, so a hung
                // connection (one slow timeout) can't stack ~3× timeouts onto the
                // serial pipeline build; a fast transient failure still retries.
                if start.elapsed() >= budget {
                    tracing::debug!("caption master fetch budget spent; not retrying");
                    break;
                }
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
            }
        }
    }
    Err(last)
}

/// Upper bound on a fetched playlist's size (a real master/rendition is a few KB).
const MAX_PLAYLIST_BYTES: usize = 8 * 1024 * 1024;

/// The per-source caption reader loop, run on a dedicated ingest thread.
///
/// Opens the resolved rendition `m3u8`, finds its subtitle stream, builds a
/// `WebVTT` [`CaptionDecoder`], pumps packets, and publishes each decoded cue
/// into the per-source [`CueStore`].
///
/// A **live** rendition (an HLS source) is **supervised-reconnected** on
/// EOF/error, mirroring the video [`ingest_loop`](crate::pipeline) bracket: a
/// transient `.vtt` 404 / token-expiry / segment blip backs off
/// (capped-exponential + per-source jitter) and retries, instead of permanently
/// killing captions for the run. A connection that streamed for a while resets
/// the escalation; a hard-down rendition never hot-loops. A **finite** VOD
/// rendition (e.g. bipbop) plays out once and then stops — we do not
/// hammer-reconnect a finished VOD; its decoded cues remain in the bounded store
/// for the baker. The loop returns promptly whenever `stop` is raised (the
/// backoff sleep is `stop`-checked).
///
/// The loop only ever *writes* the lock-free store, so it can neither pace nor
/// stall the output clock (invariant #1) nor back-pressure the engine (#10).
pub fn caption_loop(plan: &CaptionPlan, stop: &AtomicBool) {
    let mut attempt: u32 = 0;
    let mut jitter = crate::pipeline::JitterRng::seeded(&plan.id);
    loop {
        if stop.load(Ordering::Acquire) || plan.store.generation() != plan.generation {
            return;
        }
        let started = std::time::Instant::now();
        if let Err(reason) = read_captions(plan, stop) {
            tracing::warn!(source = %plan.id, %reason, "caption rendition ended/errored");
        }
        let ran_for = started.elapsed();
        // A finite (VOD) rendition plays out once; never reconnect it. A stop
        // raised while we ran ends the loop immediately.
        if !plan.live || stop.load(Ordering::Acquire) {
            return;
        }
        // Live: back off (capped-exponential + jitter) and retry, so a transient
        // rendition failure recovers instead of disabling captions for the run.
        attempt = crate::pipeline::next_reconnect_attempt(attempt, ran_for);
        let nap = crate::pipeline::reconnect_backoff(attempt, jitter.next_unit());
        tracing::debug!(
            source = %plan.id,
            attempt,
            backoff_ms = nap.as_millis(),
            "caption rendition reconnecting after a fault"
        );
        crate::pipeline::sleep_interruptible(nap, stop);
    }
}

/// Read `plan.rendition_url` — the rendition's media playlist — and publish
/// every cue of its `.vtt` segments into `plan.store`, in pure Rust (#526).
/// Returns `Ok(())` at the end of a finished (`EXT-X-ENDLIST`) playlist or on
/// stop; `Err` when the playlist cannot be fetched or parsed (the live
/// supervisor in [`caption_loop`] backs off and retries).
///
/// No libav demuxer/decoder is involved: a minimal libav build may lack the
/// `webvtt` components, and libav ignores `X-TIMESTAMP-MAP`. Each cue is put
/// on the MPEG-TS clock by its segment's `X-TIMESTAMP-MAP`
/// ([`crate::vtt_rendition`]) and projected onto the video's normalized
/// timeline through the source's [`video anchor`](CaptionPlan::video_anchor).
/// Every fetch is bounded in bytes and in total time and aborts on stop; the
/// segment schemes are limited to the rendition's own (`http(s)` for a network
/// rendition, which a network master always names: [`resolve_caption_plan`]
/// refuses a local one). Cues are mapped only through a video anchor on their
/// own clock ([`ClockMatch`]): a splice restarts the cue clock.
fn read_captions(plan: &CaptionPlan, stop: &AtomicBool) -> Result<(), String> {
    // A bare local path (a local master) becomes a `file://` URL once, so
    // relative segment URIs resolve and fetch in the same form.
    let rendition_url = if has_scheme(&plan.rendition_url) {
        plan.rendition_url.clone()
    } else {
        format!("file://{}", plan.rendition_url)
    };
    let allowed = rendition_protocols(&rendition_url)?;
    let mut clock = ClockMatch::default();
    let mut next_seq: Option<u64> = None;
    let mut pending = PendingCues::default();
    loop {
        if stop.load(Ordering::Acquire) || plan.store.generation() != plan.generation {
            return Ok(());
        }
        let fetched = fetch_rendition_text(
            &rendition_url,
            MAX_RENDITION_PLAYLIST_BYTES,
            allowed,
            &plan.http_headers,
            stop,
        )?;
        let playlist = crate::vtt_rendition::RenditionPlaylist::parse(&fetched.body)
            .ok_or_else(|| "rendition is not an HLS media playlist".to_owned())?;
        let first = first_new_segment(&playlist, next_seq, plan.live);
        for segment in playlist.segments.iter().skip(first) {
            if stop.load(Ordering::Acquire) || plan.store.generation() != plan.generation {
                return Ok(());
            }
            next_seq = Some(segment.sequence.saturating_add(1));
            let url = resolve_rendition_uri(&fetched.url, &segment.uri);
            let body = match fetch_rendition_text(
                &url,
                MAX_VTT_SEGMENT_BYTES,
                allowed,
                &plan.http_headers,
                stop,
            ) {
                Ok(fetched) => fetched.body,
                Err(reason) => {
                    // One missing segment loses its cues, never the reader nor
                    // its position: its listed duration still advances it.
                    tracing::debug!(source = %plan.id, %reason, "caption segment fetch failed; skipped");
                    clock.advance(segment, None);
                    continue;
                }
            };
            let Some(cues) = crate::vtt_rendition::parse_segment(&body) else {
                tracing::debug!(source = %plan.id, "caption segment is not WebVTT; skipped");
                clock.advance(segment, None);
                continue;
            };
            // Map the segment through the video mapping of ITS discontinuity
            // epoch (see [`ClockMatch`]).
            let anchor = match clock.segment(plan, segment, cues.first(), stop) {
                SegmentMapping::Map(anchor) => anchor,
                SegmentMapping::Skip => {
                    pending.dropped = pending
                        .dropped
                        .saturating_add(u64::try_from(cues.len()).unwrap_or(u64::MAX));
                    pending.report_drops(&plan.id);
                    clock.advance(segment, None);
                    continue;
                }
                SegmentMapping::Stop => return Ok(()),
            };
            let mapped: Vec<CaptionCue> =
                cues.iter().filter_map(|cue| map_cue(anchor, cue)).collect();
            let latest_start = mapped.iter().map(CaptionCue::start).max();
            let earliest_end = mapped.iter().map(CaptionCue::end).min();
            clock.advance(segment, latest_start.zip(earliest_end));
            // Feed the bounded pending window in chunks it always holds whole
            // (a 5 s live segment carries ~25 cues; the window keeps 16), so a
            // dense rolling-caption segment loses none to the cap.
            let mut mapped = mapped.into_iter().peekable();
            while mapped.peek().is_some() {
                let chunk: Vec<CaptionCue> = mapped.by_ref().take(PENDING_CUE_CAP / 2).collect();
                publish_cues(plan, &mut pending, chunk, stop);
            }
        }
        if playlist.ended {
            return Ok(());
        }
        let poll = std::time::Duration::from_secs(playlist.target_duration_s.clamp(2, 12) / 2);
        crate::pipeline::sleep_interruptible(poll, stop);
    }
}

/// Live segments a fresh reader starts behind the playlist's edge (the video
/// demux, like libav's default, also starts a few segments back).
const LIVE_START_SEGMENTS: usize = 3;
/// Byte bound of one rendition media playlist fetch.
const MAX_RENDITION_PLAYLIST_BYTES: usize = 1024 * 1024;
/// Byte bound of one `.vtt` segment fetch (a 5 s live segment is ~3 KiB).
const MAX_VTT_SEGMENT_BYTES: usize = 512 * 1024;

/// The index of the first segment of `playlist` not yet read: after
/// `next_seq`, or (a fresh reader) [`LIVE_START_SEGMENTS`] behind a live edge /
/// the start of a finished playlist. A media sequence that moved backwards past
/// the whole window (an origin restart) restarts at the live edge.
fn first_new_segment(
    playlist: &crate::vtt_rendition::RenditionPlaylist,
    next_seq: Option<u64>,
    live: bool,
) -> usize {
    let edge = if live && !playlist.ended {
        playlist.segments.len().saturating_sub(LIVE_START_SEGMENTS)
    } else {
        0
    };
    let Some(next) = next_seq else {
        return edge;
    };
    let last = playlist.segments.last().map(|s| s.sequence);
    if last.is_some_and(|last| last.saturating_add(1) < next) {
        return edge;
    }
    playlist
        .segments
        .iter()
        .position(|s| s.sequence >= next)
        .unwrap_or(playlist.segments.len())
}

/// Whether `url` is an `http(s)` URL.
fn is_network_url(url: &str) -> bool {
    matches!(
        url.split_once("://").map(|(scheme, _)| scheme),
        Some("http" | "https")
    )
}

/// The libav protocols a rendition's playlist and segments may use: the
/// network set for an `http(s)` rendition, `file` only for a local one (a
/// local master). Anything else is refused.
fn rendition_protocols(url: &str) -> Result<&'static str, String> {
    let scheme = url.split_once("://").map(|(scheme, _)| scheme);
    match scheme {
        Some("http" | "https") => Ok(ALLOWED_PROTOCOLS),
        Some("file") | None => Ok("file"),
        Some(other) => Err(format!("rendition scheme {other:?} is not supported")),
    }
}

/// Fetch one rendition text resource (bounded bytes, bounded time). A bare
/// local path is read as `file:`.
fn fetch_rendition_text(
    url: &str,
    max_bytes: usize,
    allowed: &str,
    headers: &multiview_ffmpeg::HttpHeaders,
    stop: &AtomicBool,
) -> Result<multiview_ffmpeg::FetchedText, String> {
    let limits = multiview_ffmpeg::FetchLimits {
        budget: Some(RENDITION_FETCH_BUDGET),
        stop: Some(stop),
    };
    multiview_ffmpeg::fetch_url_text_with_headers(url, max_bytes, allowed, headers, &limits)
        .map_err(|e| e.to_string())
}

/// Total time budget of one rendition playlist/segment fetch (a trickling
/// server cannot hold the reader past it; a raised stop aborts at once).
const RENDITION_FETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// With no position estimate (a reader's first segment, or after a sequence
/// gap), a cue maps through the video's current mapping when that puts it
/// within this distance of the shown picture.
const SAME_CLOCK_NS: i64 = 60_000_000_000;
/// The longest a segment waits for the video to resolve its mapping
/// ([`ClockMatch`]): a reader runs a live window (a few segments) ahead.
const CLOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// How a rendition chunk's cues map onto the video timeline: through a live
/// anchor `(raw0, pts0)`, or (a file/VOD source, no live anchor) as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CueAnchor {
    Raw { raw0: i64, pts0: MediaTime },
    AsWritten,
}

/// The reader's correlation of rendition discontinuity epochs with the
/// video's raw <-> normalized mappings (#526 review). An `EXT-X-DISCONTINUITY`
/// (an ad splice) may restart the cue clock, and raw times are then REUSED, so
/// no raw-time comparison can tell which mapping a new epoch's cues belong to.
/// What identifies it is WHERE the video is: the normalized timeline the
/// selected picture carries is continuous across the video's rebases, and the
/// rendition's `#EXTINF` durations locate every segment's start on it.
///
/// So the reader keeps an interval estimate `[lo, hi]` of the normalized start
/// of the next segment it reads: calibrated by every mapped cue (a cue of a
/// segment starts before the segment ends and ends after it starts; it may
/// start segments earlier) and advanced by each segment's duration. A segment
/// maps:
///
/// * of the epoch already mapped: through that epoch's stored mapping;
///   an older epoch: dropped;
/// * of a newer epoch: through the SHOWN picture's own mapping (the latest
///   anchor, once it is not ahead of the picture), and only while the picture
///   is provably in the segment's epoch: before the segment's earliest
///   possible end (`lo + duration`, so not in a later epoch) and either past
///   its latest possible start (`hi`) or, for the very next epoch, past `lo`
///   with a mapping CHANGED from the previous epoch's (the video rebased: it
///   crossed; an epoch's timestamps are continuous, so it never rebases
///   inside one). A picture past the segment's earliest end skips it (fail
///   closed: its epoch is unknown);
/// * the first one, or after a sequence gap (the window slid past segments
///   this reader never saw listed; a failed fetch is no gap, its listed
///   duration still advances the estimate): a re-synchronisation through the
///   shown picture's mapping when it puts the cues near that picture. This is
///   the one place timestamps decide (as any HLS player syncing a subtitle
///   rendition to its video does): a (re)sync landing within
///   [`SAME_CLOCK_NS`] of a splice that reused the raw clock can pick the
///   wrong side; it is re-decided at the next epoch.
///
/// This never maps a new epoch through a mapping of the wrong side of a
/// splice; the price is timeliness when the splice KEEPS the raw clock (no
/// rebase to observe): its first cues wait until the picture is surely past
/// the segment start (the estimate's width, at most about one segment
/// duration, or more after a long carried-over cue) and may expire. Exact
/// correlation needs the video's own discontinuity position (tracked in the
/// PR's Concerns).
///
/// The mapping is identified by its offset ([`same_mapping`]), never by the
/// [`LiveAnchor`] generation. Every wait is bounded ([`CLOCK_WAIT`]) and
/// stop-checked. A live source whose anchor slot holds only the lifecycle
/// placeholder (no raw PTS yet) waits; an empty slot (a file/VOD rendition)
/// plays cues as written.
///
/// [`LiveAnchor`]: crate::pipeline::LiveAnchor
#[derive(Debug, Default)]
struct ClockMatch {
    /// The last mapped epoch and its mapping.
    mapped: Option<(u64, CueAnchor)>,
    /// The normalized `[lo, hi]` start estimate of the segment `next_seq`.
    next_start: Option<(i64, i64)>,
    /// The media sequence the estimate belongs to.
    next_seq: Option<u64>,
}

/// Slack added to a segment's latest start before a new epoch is taken
/// (`#EXTINF` rounding against the decoded media).
const EPOCH_SLACK_NS: i64 = 500_000_000;

/// What the reader does with one segment's cues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentMapping {
    /// Map its cues through this mapping.
    Map(CueAnchor),
    /// Skip its cues (stale, or unresolvable within the bound).
    Skip,
    /// Stop reading (stop raised or a newer generation).
    Stop,
}

impl ClockMatch {
    /// Decide (waiting off the output clock, bounded, stop-checked) how the
    /// cues of `segment` map. `first` is its first cue, if any.
    fn segment(
        &mut self,
        plan: &CaptionPlan,
        segment: &crate::vtt_rendition::Segment,
        first: Option<&crate::vtt_rendition::VttCue>,
        stop: &AtomicBool,
    ) -> SegmentMapping {
        self.continuity(segment);
        let Some(first) = first else {
            return SegmentMapping::Skip;
        };
        match self.mapped {
            Some((epoch, anchor)) if epoch == segment.discontinuity => {
                return SegmentMapping::Map(anchor)
            }
            Some((epoch, _)) if segment.discontinuity < epoch => return SegmentMapping::Skip,
            _ => {}
        }
        // When the segment opens a new epoch after a mapped one: its start
        // estimate, the previous mapping, and whether it is the very next epoch.
        let crossing = match (self.mapped, self.next_start) {
            (Some((epoch, previous)), Some((lo, hi))) => Some(Crossing {
                lo,
                hi,
                previous,
                adjacent: segment.discontinuity == epoch.saturating_add(1),
            }),
            _ => None,
        };
        let started = std::time::Instant::now();
        loop {
            if stop.load(Ordering::Acquire) || plan.store.generation() != plan.generation {
                return SegmentMapping::Stop;
            }
            match Self::look(plan, first, crossing, segment.duration_ns) {
                Look::Take(anchor) => {
                    self.mapped = Some((segment.discontinuity, anchor));
                    return SegmentMapping::Map(anchor);
                }
                Look::Stale => return SegmentMapping::Skip,
                Look::Wait => {}
            }
            if started.elapsed() >= CLOCK_WAIT {
                return SegmentMapping::Skip;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The first segment, or a sequence gap (the live window slid past
    /// segments never listed to this reader, an origin restart): no position
    /// estimate and no epoch mapping, so the reader re-synchronises as on its
    /// first segment. Idempotent for the segment being processed.
    fn continuity(&mut self, segment: &crate::vtt_rendition::Segment) {
        if self.next_seq != Some(segment.sequence) {
            self.next_start = None;
            self.mapped = None;
            self.next_seq = Some(segment.sequence);
        }
    }

    /// One look at the video for a segment whose first cue is `first`
    /// (`crossing`: see [`Crossing`]; `None` is the near-picture rule).
    fn look(
        plan: &CaptionPlan,
        first: &crate::vtt_rendition::VttCue,
        crossing: Option<Crossing>,
        duration_ns: i64,
    ) -> Look {
        let Some(shown) = plan
            .displayed_time
            .load()
            .filter(|t| t.generation == plan.generation)
        else {
            return Look::Wait;
        };
        let shown = shown.pts.as_nanos();
        let Some(anchor) = plan.video_anchor.load() else {
            return Look::Take(CueAnchor::AsWritten);
        };
        let Some(raw0) = anchor.raw_pts0_ns else {
            return Look::Wait;
        };
        // The latest anchor is the SHOWN picture's mapping only when it is
        // not ahead of that picture (anchors are published in normalized
        // order, up to the jitter hold before their picture is shown).
        if anchor.pts0.as_nanos() > shown {
            return Look::Wait;
        }
        let current = CueAnchor::Raw {
            raw0,
            pts0: anchor.pts0,
        };
        if let Some(Crossing {
            lo,
            hi,
            previous,
            adjacent,
        }) = crossing
        {
            // The picture may already be past the segment (its earliest
            // possible end): its epoch is unknown, fail closed.
            let earliest_end = lo.saturating_add(duration_ns);
            if shown >= earliest_end {
                return Look::Stale;
            }
            // Before the segment's earliest end, the picture is in this
            // segment's epoch or an earlier one. It is surely in this one when
            // past the segment's latest start, or (the very next epoch) when
            // the video rebased since the previous epoch's mapping: an epoch's
            // timestamps are continuous, so it rebases only at a splice.
            let surely = shown >= hi.saturating_add(EPOCH_SLACK_NS);
            let rebased = adjacent
                && shown >= lo.saturating_sub(EPOCH_SLACK_NS)
                && !same_mapping(previous, current);
            return if surely || rebased {
                Look::Take(current)
            } else {
                Look::Wait
            };
        }
        let at = crate::vtt_rendition::project_raw(first.raw_start, raw0, anchor.pts0);
        if at.as_nanos().saturating_sub(shown).abs() <= SAME_CLOCK_NS {
            Look::Take(current)
        } else {
            Look::Wait
        }
    }

    /// Record a read segment: calibrate the start estimate with its mapped
    /// cues (`(latest cue start, earliest cue end)` on the normalized
    /// timeline, `None` when not mapped) and advance it past the segment.
    /// A segment carries every cue active in it, with full times (a cue may
    /// start segments earlier): each cue starts before the segment ends and
    /// ends after it starts, so `latest start - duration < segment start <
    /// earliest end`.
    fn advance(
        &mut self,
        segment: &crate::vtt_rendition::Segment,
        mapped: Option<(MediaTime, MediaTime)>,
    ) {
        // Every segment path (mapped, skipped, failed fetch, not WebVTT)
        // records through here, so a gap is never concealed.
        self.continuity(segment);
        let duration = segment.duration_ns;
        let mut estimate = self.next_start;
        if let Some((latest_start, earliest_end)) = mapped {
            let lo = latest_start.as_nanos().saturating_sub(duration);
            let hi = earliest_end.as_nanos();
            estimate = match estimate {
                Some((old_lo, old_hi)) if old_lo.max(lo) <= old_hi.min(hi) => {
                    Some((old_lo.max(lo), old_hi.min(hi)))
                }
                _ => Some((lo.min(hi), hi)),
            };
        }
        self.next_start =
            estimate.map(|(lo, hi)| (lo.saturating_add(duration), hi.saturating_add(duration)));
        self.next_seq = Some(segment.sequence.saturating_add(1));
    }
}

/// A segment that opens a new discontinuity epoch after a mapped one.
#[derive(Debug, Clone, Copy)]
struct Crossing {
    /// Its earliest possible normalized start.
    lo: i64,
    /// Its latest possible normalized start.
    hi: i64,
    /// The mapping of the previous mapped epoch.
    previous: CueAnchor,
    /// Whether it is the epoch right after the mapped one.
    adjacent: bool,
}

/// One look's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Take(CueAnchor),
    Stale,
    Wait,
}

/// Whether two mappings are the same raw <-> normalized offset (within 1 ms,
/// modulo the 33-bit wrap): an anchor republished without a rebase.
fn same_mapping(a: CueAnchor, b: CueAnchor) -> bool {
    match (a, b) {
        (CueAnchor::AsWritten, CueAnchor::AsWritten) => true,
        (
            CueAnchor::Raw { raw0, pts0 },
            CueAnchor::Raw {
                raw0: raw1,
                pts0: pts1,
            },
        ) => {
            let at = crate::vtt_rendition::project_raw(raw1, raw0, pts0);
            at.as_nanos().saturating_sub(pts1.as_nanos()).abs() <= 1_000_000
        }
        _ => false,
    }
}

/// Place a rendition cue on the video's normalized timeline through `anchor`.
fn map_cue(anchor: CueAnchor, cue: &crate::vtt_rendition::VttCue) -> Option<CaptionCue> {
    let (start, end) = match anchor {
        CueAnchor::Raw { raw0, pts0 } => (
            crate::vtt_rendition::project_raw(cue.raw_start, raw0, pts0),
            crate::vtt_rendition::project_raw(cue.raw_end, raw0, pts0),
        ),
        CueAnchor::AsWritten => (cue.local_start, cue.local_end),
    };
    CaptionCue::try_text(start, end, cue.lines.clone(), None).ok()
}

/// Hard caps for decoded cues retained between packets. A single libav decode
/// can still allocate its own vector before this function receives it.
const PENDING_CUE_CAP: usize = 16;
const PENDING_BYTES_CAP: usize = 256 * 1024;
const CUE_AHEAD_NS: i64 = 2_000_000_000;
const CUE_FUTURE_NS: i64 = 120_000_000_000;

#[derive(Default)]
struct PendingCues {
    cues: Vec<CaptionCue>,
    bytes: usize,
    dropped: u64,
    reported_at: Option<std::time::Instant>,
}

fn cue_bytes(cue: &CaptionCue) -> usize {
    match cue {
        CaptionCue::Text { text, .. } => text
            .lines
            .iter()
            .fold(0_usize, |n, line| n.saturating_add(line.len())),
        CaptionCue::Bitmap { bitmap, .. } => bitmap.rgba.len(),
        _ => usize::MAX, // Unknown future cue shapes are not retained.
    }
}

impl PendingCues {
    fn push(&mut self, cue: CaptionCue, cursor: Option<MediaTime>) {
        let size = cue_bytes(&cue);
        let too_far = cursor
            .is_some_and(|at| cue.start().as_nanos() > at.as_nanos().saturating_add(CUE_FUTURE_NS));
        let stale = cursor.is_some_and(|at| cue.end() <= at);
        if size > PENDING_BYTES_CAP || too_far || stale {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let pos = self
            .cues
            .partition_point(|entry| entry.start() <= cue.start());
        self.cues.insert(pos, cue);
        self.bytes = self.bytes.saturating_add(size);
        while self.cues.len() > PENDING_CUE_CAP || self.bytes > PENDING_BYTES_CAP {
            // Keep the active/next earliest cue when a malicious packet carries
            // many future cues. Drop farthest-future first.
            if let Some(removed) = self.cues.pop() {
                self.bytes = self.bytes.saturating_sub(cue_bytes(&removed));
                self.dropped = self.dropped.saturating_add(1);
            }
        }
    }

    fn flush(&mut self, plan: &CaptionPlan) {
        let Some(displayed) = plan.displayed_time.load() else {
            return;
        };
        if displayed.generation != plan.generation {
            return;
        }
        let horizon = displayed.pts.as_nanos().saturating_add(CUE_AHEAD_NS);
        while self
            .cues
            .first()
            .is_some_and(|cue| cue.start().as_nanos() <= horizon)
        {
            let cue = self.cues.remove(0);
            self.bytes = self.bytes.saturating_sub(cue_bytes(&cue));
            if cue.end() <= displayed.pts {
                self.dropped = self.dropped.saturating_add(1);
            } else {
                plan.store
                    .publish_for_generation(plan.generation, cue.start(), cue.end(), cue);
            }
        }
    }

    fn report_drops(&mut self, id: &str) {
        if self.dropped > 0
            && self
                .reported_at
                .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(5))
        {
            tracing::warn!(source = %id, dropped = self.dropped, "caption cues discarded by bounded reader window");
            self.dropped = 0;
            self.reported_at = Some(std::time::Instant::now());
        }
    }
}

/// Keep decoded cue history near the selected picture. This runs only on the
/// isolated reader; the output clock never waits on an input or this queue.
fn publish_cues(
    plan: &CaptionPlan,
    pending: &mut PendingCues,
    cues: Vec<CaptionCue>,
    stop: &AtomicBool,
) {
    if stop.load(Ordering::Acquire) || plan.store.generation() != plan.generation {
        return;
    }
    let cursor = plan
        .displayed_time
        .load()
        .filter(|t| t.generation == plan.generation)
        .map(|t| t.pts);
    for cue in cues {
        pending.push(cue, cursor);
    }
    pending.flush(plan);
    pending.report_drops(&plan.id);
    while !stop.load(Ordering::Acquire)
        && plan.store.generation() == plan.generation
        && !pending.cues.is_empty()
    {
        pending.flush(plan);
        if pending.cues.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// RFC 3986 §5.2.4 `remove_dot_segments` on a path, leaving any `?query` /
/// `#fragment` suffix untouched. `..` above the root stops at the root.
fn remove_dot_segments(path_and_rest: &str) -> String {
    let split = path_and_rest
        .find(['?', '#'])
        .unwrap_or(path_and_rest.len());
    let (path, rest) = path_and_rest.split_at(split);
    let absolute = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').collect();
    let last = segments.len().saturating_sub(1);
    let mut trailing_dir = false;
    for (i, seg) in segments.iter().enumerate() {
        match *seg {
            "." => trailing_dir = i == last,
            ".." => {
                // Never pop the leading empty segment of an absolute path.
                if out.len() > usize::from(absolute) {
                    out.pop();
                }
                trailing_dir = i == last;
            }
            "" if i == 0 && absolute => out.push(""),
            other => {
                out.push(other);
                trailing_dir = false;
            }
        }
    }
    let mut joined = out.join("/");
    if absolute && joined.is_empty() {
        joined.push('/');
    }
    if trailing_dir && !joined.ends_with('/') {
        joined.push('/');
    }
    format!("{joined}{rest}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn relative_rendition_uri_joins_against_the_master_directory() {
        // The bipbop case: a relative rendition URI under the master's directory.
        let master =
            "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8";
        let got = resolve_rendition_uri(master, "subtitles/eng/prog_index.m3u8");
        assert_eq!(
            got,
            "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/subtitles/eng/prog_index.m3u8"
        );
    }

    #[test]
    fn root_relative_rendition_uri_replaces_the_master_path() {
        let master = "https://host.example/a/b/c/master.m3u8";
        let got = resolve_rendition_uri(master, "/subs/en/index.m3u8");
        assert_eq!(got, "https://host.example/subs/en/index.m3u8");
    }

    #[test]
    fn absolute_rendition_uri_is_used_verbatim() {
        let master = "https://host.example/a/master.m3u8";
        let got = resolve_rendition_uri(master, "https://cdn.other/subs/index.m3u8");
        assert_eq!(got, "https://cdn.other/subs/index.m3u8");
    }

    #[test]
    fn master_query_and_fragment_are_dropped_before_joining() {
        let master = "https://host.example/a/master.m3u8?token=xyz#frag";
        let got = resolve_rendition_uri(master, "subs/index.m3u8");
        assert_eq!(got, "https://host.example/a/subs/index.m3u8");
    }

    #[test]
    fn dot_dot_segments_are_removed_rfc3986() {
        // AWS MediaTailor masters (e.g. CNN FAST) list variants as
        // `../../../manifest/...`; CloudFront rejects a literal `..` with 400,
        // so the joined path must have its dot-segments removed (RFC 3986 §5.2.4).
        let master = "https://d.example/v1/master/acct/chan/CNNFAST_GB.m3u8";
        let got = resolve_rendition_uri(master, "../../../manifest/acct/chan/sess/3.m3u8");
        assert_eq!(got, "https://d.example/v1/manifest/acct/chan/sess/3.m3u8");
    }

    #[test]
    fn single_dot_and_excess_dot_dot_segments_are_normalised() {
        let master = "https://h.example/a/b/master.m3u8";
        assert_eq!(
            resolve_rendition_uri(master, "./v/./index.m3u8"),
            "https://h.example/a/b/v/index.m3u8"
        );
        // More `..` than path depth stops at the root (RFC 3986 §5.2.4).
        assert_eq!(
            resolve_rendition_uri(master, "../../../../x/index.m3u8"),
            "https://h.example/x/index.m3u8"
        );
        // A rendition query string survives normalisation.
        assert_eq!(
            resolve_rendition_uri(master, "../v/index.m3u8?sid=1&a=..%2F"),
            "https://h.example/a/v/index.m3u8?sid=1&a=..%2F"
        );
    }

    #[test]
    fn root_relative_dot_segments_are_normalised() {
        let master = "https://h.example/a/master.m3u8";
        assert_eq!(
            resolve_rendition_uri(master, "/x/../y/index.m3u8"),
            "https://h.example/y/index.m3u8"
        );
    }

    #[test]
    fn bare_local_master_directory_join() {
        // No scheme://authority — a simple directory join still works.
        let master = "/srv/streams/master.m3u8";
        let got = resolve_rendition_uri(master, "subs/index.m3u8");
        assert_eq!(got, "/srv/streams/subs/index.m3u8");
        let abs = resolve_rendition_uri(master, "/abs/index.m3u8");
        assert_eq!(abs, "/abs/index.m3u8");
    }

    #[test]
    fn dvbsub_selector_only_for_ts_or_file_auto_or_track() {
        use multiview_config::SourceKind;
        let ts = SourceKind::Ts {
            url: "udp://x".to_owned(),
        };
        let file = SourceKind::File {
            path: "/x.ts".to_owned(),
        };
        let hls = SourceKind::Hls {
            url: "https://x/m.m3u8".to_owned(),
            user_agent: None,
            referrer: None,
        };
        assert!(dvbsub_selector(&ts, &CaptionSelector::Auto));
        assert!(dvbsub_selector(
            &file,
            &CaptionSelector::Track {
                id: "eng".to_owned()
            }
        ));
        // Teletext stays out (it is the teletext decoder, not dvbsub).
        assert!(!dvbsub_selector(
            &ts,
            &CaptionSelector::TeletextPage { page: 801 }
        ));
        assert!(!dvbsub_selector(&file, &CaptionSelector::Off));
        // HLS is the WebVTT-rendition path, not the in-container dvbsub path.
        assert!(!dvbsub_selector(&hls, &CaptionSelector::Auto));
    }

    #[test]
    fn incontainer_selector_active_for_ts_or_file_auto_track_or_teletext() {
        use multiview_config::SourceKind;
        let ts = SourceKind::Ts {
            url: "udp://x".to_owned(),
        };
        let file = SourceKind::File {
            path: "/x.ts".to_owned(),
        };
        let hls = SourceKind::Hls {
            url: "https://x/m.m3u8".to_owned(),
            user_agent: None,
            referrer: None,
        };
        assert!(incontainer_selector_active(&ts, &CaptionSelector::Auto));
        assert!(incontainer_selector_active(
            &file,
            &CaptionSelector::Track {
                id: "eng".to_owned()
            }
        ));
        // Teletext IS the in-container path (it is a muxed PID, decoded in-demux).
        assert!(incontainer_selector_active(
            &ts,
            &CaptionSelector::TeletextPage { page: 801 }
        ));
        // Off / embedded-cc / sidecar are not this path.
        assert!(!incontainer_selector_active(&file, &CaptionSelector::Off));
        assert!(!incontainer_selector_active(
            &ts,
            &CaptionSelector::EmbeddedCc {
                field: "cc1".to_owned()
            }
        ));
        // HLS takes the WebVTT-rendition path, not the in-container path.
        assert!(!incontainer_selector_active(&hls, &CaptionSelector::Auto));
    }

    #[test]
    fn incontainer_caption_source_maps_every_supported_codec() {
        // Auto/Track map by codec — none of the decoder's in-container forms may be
        // silently dropped at the wiring layer.
        let auto = CaptionSelector::Auto;
        assert_eq!(
            incontainer_caption_source(&auto, "dvbsub"),
            Some(CaptionSource::DvbSubtitle)
        );
        assert_eq!(
            incontainer_caption_source(&auto, "ass"),
            Some(CaptionSource::Ass)
        );
        assert_eq!(
            incontainer_caption_source(&auto, "ssa"),
            Some(CaptionSource::Ass)
        );
        assert_eq!(
            incontainer_caption_source(&auto, "subrip"),
            Some(CaptionSource::SubRip)
        );
        assert_eq!(
            incontainer_caption_source(&auto, "mov_text"),
            Some(CaptionSource::MovText)
        );
        assert_eq!(
            incontainer_caption_source(&auto, "dvb_teletext"),
            Some(CaptionSource::Teletext { page: None })
        );
        // A codec this path does not decode declines (no wrong/empty decoder).
        assert_eq!(incontainer_caption_source(&auto, "hdmv_pgs_subtitle"), None);
        // TeletextPage pins the page, but only over a real teletext stream.
        let tt = CaptionSelector::TeletextPage { page: 801 };
        assert_eq!(
            incontainer_caption_source(&tt, "dvb_teletext"),
            Some(CaptionSource::Teletext { page: Some(801) })
        );
        assert_eq!(
            incontainer_caption_source(&tt, "dvbsub"),
            None,
            "a teletext_page selector over a non-teletext stream must decline"
        );
        // Off / embedded-cc / sidecar are not this path.
        assert_eq!(
            incontainer_caption_source(&CaptionSelector::Off, "ass"),
            None
        );
    }

    #[test]
    fn embedded_cc_channel_maps_608_fields_and_refuses_708_and_non_video_kinds() {
        use multiview_config::SourceKind;
        use multiview_ffmpeg::CcChannel;
        let ts = SourceKind::Ts {
            url: "udp://x".to_owned(),
        };
        let field = |f: &str| CaptionSelector::EmbeddedCc {
            field: f.to_owned(),
        };
        // Each 608 field maps (case-insensitively).
        assert_eq!(
            embedded_cc_channel(&ts, &field("cc1")),
            Some(CcChannel::Cc1)
        );
        assert_eq!(
            embedded_cc_channel(&ts, &field("CC2")),
            Some(CcChannel::Cc2)
        );
        assert_eq!(
            embedded_cc_channel(&ts, &field("cc3")),
            Some(CcChannel::Cc3)
        );
        assert_eq!(
            embedded_cc_channel(&ts, &field("cc4")),
            Some(CcChannel::Cc4)
        );
        // A 708 service is refused honestly (the linked cc_dec has no 708 text).
        assert_eq!(embedded_cc_channel(&ts, &field("service:1")), None);
        assert_eq!(embedded_cc_channel(&ts, &field("708:1")), None);
        // An unrecognised field declines.
        assert_eq!(embedded_cc_channel(&ts, &field("nonsense")), None);
        // Embedded CC rides the video, so it is offered for any video-decoded kind.
        let hls = SourceKind::Hls {
            url: "https://x/m.m3u8".to_owned(),
            user_agent: None,
            referrer: None,
        };
        assert_eq!(
            embedded_cc_channel(&hls, &field("cc1")),
            Some(CcChannel::Cc1)
        );
        // A synthetic source has no video stream / A53 — declines.
        assert_eq!(embedded_cc_channel(&SourceKind::Bars, &field("cc1")), None);
        // A non-embedded selector is not this path.
        assert_eq!(embedded_cc_channel(&ts, &CaptionSelector::Auto), None);
    }

    #[test]
    fn webvtt_language_only_for_hls_auto_or_track() {
        use multiview_config::SourceKind;
        let hls = SourceKind::Hls {
            url: "https://x/master.m3u8".to_owned(),
            user_agent: None,
            referrer: None,
        };
        assert_eq!(webvtt_language(&hls, &CaptionSelector::Auto), Some(None));
        assert_eq!(
            webvtt_language(
                &hls,
                &CaptionSelector::Track {
                    id: "en".to_owned()
                }
            ),
            Some(Some("en".to_owned()))
        );
        assert_eq!(webvtt_language(&hls, &CaptionSelector::Off), None);
        // A non-HLS source never takes the HLS-rendition path.
        let file = SourceKind::File {
            path: "/x.ts".to_owned(),
        };
        assert_eq!(webvtt_language(&file, &CaptionSelector::Auto), None);
    }

    /// A [`PlaylistFetcher`] returning a fixed canned body, echoing the requested
    /// URL as the effective URL (no redirect) — drives the fetch→parse→pick→resolve
    /// seam offline, no network, no FFI.
    struct CannedFetcher(Result<String, String>);
    impl PlaylistFetcher for CannedFetcher {
        fn fetch(&self, url: &str) -> Result<FetchedPlaylist, String> {
            self.0.clone().map(|body| FetchedPlaylist {
                url: url.to_owned(),
                body,
            })
        }
    }

    /// A [`PlaylistFetcher`] that simulates a redirecting/CDN-fronted master: the
    /// requested URL is ignored and the canned body is reported as having been
    /// fetched from a different **effective** URL (the post-redirect location).
    /// Relative child URIs must resolve against this effective base, never the
    /// requested one (the ABC/Akamai footgun).
    struct RedirectingFetcher {
        effective_url: String,
        body: String,
    }
    impl PlaylistFetcher for RedirectingFetcher {
        fn fetch(&self, _url: &str) -> Result<FetchedPlaylist, String> {
            Ok(FetchedPlaylist {
                url: self.effective_url.clone(),
                body: self.body.clone(),
            })
        }
    }

    const MASTER_WITH_SUBS: &str = concat!(
        "#EXTM3U\n",
        "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",",
        "LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,",
        "URI=\"subtitles/eng/prog_index.m3u8\"\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=1000000,SUBTITLES=\"subs\"\n",
        "video/prog_index.m3u8\n",
    );

    #[test]
    fn resolves_the_subtitle_rendition_from_a_fetched_master() {
        let fetcher = CannedFetcher(Ok(MASTER_WITH_SUBS.to_owned()));
        let plan = resolve_caption_plan(
            "cam",
            "https://h.test/live/master.m3u8",
            Some("en"),
            &fetcher,
        )
        .expect("a master with an English SUBTITLES rendition resolves a plan");
        assert_eq!(plan.id, "cam");
        assert!(
            plan.rendition_url
                .ends_with("subtitles/eng/prog_index.m3u8"),
            "rendition_url = {}",
            plan.rendition_url
        );
    }

    #[test]
    fn a_relative_subtitle_rendition_resolves_against_the_redirected_master_base() {
        // The ABC/Akamai footgun: the requested `c.mjh.nz` master 302-redirects to
        // a signed Akamai master whose SUBTITLES `URI` is relative. The rendition
        // must resolve against the EFFECTIVE (post-redirect) Akamai base, NOT the
        // requested `c.mjh.nz` origin (which would 404).
        let fetcher = RedirectingFetcher {
            effective_url: "https://abc.akamaized.net/out/v1/abcd/master.m3u8?hdnea=token"
                .to_owned(),
            body: MASTER_WITH_SUBS.to_owned(),
        };
        let plan = resolve_caption_plan(
            "abc",
            "https://c.mjh.nz/abc-news.m3u8",
            Some("en"),
            &fetcher,
        )
        .expect("a redirected master with an English SUBTITLES rendition resolves a plan");
        assert_eq!(
            plan.rendition_url,
            "https://abc.akamaized.net/out/v1/abcd/subtitles/eng/prog_index.m3u8",
            "the relative SUBTITLES URI must resolve under the post-redirect Akamai base, \
             not the requested c.mjh.nz origin"
        );
    }

    #[test]
    fn a_persistent_fetch_failure_yields_no_plan_not_a_panic() {
        // Best-effort contract: a *persistent* fetch failure still yields None and
        // never panics or fails the live source's build (this behaviour is
        // intentionally preserved). The actual empty-band regression — a
        // *transient* blip disabling captions for the run — is fixed by the retry
        // and guarded by `fetch_with_retry_recovers_after_transient_failures`.
        let fetcher = CannedFetcher(Err("connection refused".to_owned()));
        let plan = resolve_caption_plan(
            "cam",
            "https://h.test/live/master.m3u8",
            Some("en"),
            &fetcher,
        );
        assert!(
            plan.is_none(),
            "a persistent fetch failure must yield no plan"
        );
    }

    #[test]
    fn fetch_with_retry_recovers_after_transient_failures() {
        use std::cell::Cell;
        let calls = Cell::new(0_u32);
        let result = fetch_with_retry(
            || {
                let n = calls.get() + 1;
                calls.set(n);
                if n < 3 {
                    Err(format!("blip {n}"))
                } else {
                    Ok("recovered".to_owned())
                }
            },
            3,
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(3600),
        );
        assert_eq!(result.as_deref(), Ok("recovered"));
        assert_eq!(calls.get(), 3, "must retry until success");
    }

    #[test]
    fn fetch_with_retry_gives_up_after_the_bound_with_the_last_error() {
        use std::cell::Cell;
        let calls = Cell::new(0_u32);
        let result: Result<String, String> = fetch_with_retry(
            || {
                calls.set(calls.get() + 1);
                Err(format!("fail {}", calls.get()))
            },
            3,
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(3600),
        );
        assert_eq!(result, Err("fail 3".to_owned()));
        assert_eq!(calls.get(), 3, "must stop at the attempt bound");
    }

    #[test]
    fn fetch_with_retry_stops_early_when_the_time_budget_is_spent() {
        use std::cell::Cell;
        // A zero budget models a hung attempt that ate the whole budget: no retry
        // follows, so a dead/hung master can't stack timeouts onto the build.
        let calls = Cell::new(0_u32);
        let result: Result<String, String> = fetch_with_retry(
            || {
                calls.set(calls.get() + 1);
                Err("hung".to_owned())
            },
            5,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        );
        assert!(result.is_err());
        assert_eq!(
            calls.get(),
            1,
            "a spent budget must stop after the first attempt"
        );
    }

    #[test]
    fn par_filter_map_runs_every_closure_concurrently_and_drops_none() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        let n = 4usize;
        // A barrier of `n` parties only releases once all `n` closures are in
        // flight at the same instant — so `par_filter_map` returning at all proves
        // it ran them concurrently (one thread per item, no serial blocking). The
        // peak counter records that concurrency for the assertion.
        let barrier = Barrier::new(n);
        let peak = AtomicUsize::new(0);
        let in_flight = AtomicUsize::new(0);
        let items: Vec<usize> = (0..n).collect();

        let mut out = par_filter_map(&items, |&i| {
            let now = in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(now, Ordering::AcqRel);
            barrier.wait();
            in_flight.fetch_sub(1, Ordering::AcqRel);
            (i % 2 == 0).then_some(i * 10)
        });
        out.sort_unstable();

        assert_eq!(
            peak.load(Ordering::Acquire),
            n,
            "every closure must run concurrently (one thread per item)"
        );
        assert_eq!(out, vec![0, 20], "odd items map to None and are dropped");
    }

    #[test]
    fn par_filter_map_on_empty_input_is_empty() {
        let out = par_filter_map::<u8, u8, _>(&[], |_| Some(1));
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn resolve_caption_plans_with_resolves_every_captioned_hls_source() {
        // Two HLS sources with captions resolve a plan each; a third HLS source
        // with captions explicitly OFF resolves none (proving the filter on the
        // real Source path). The fetches run concurrently (par_filter_map), off
        // the serial build path (#48).
        let toml = r##"
schema_version = 1
[canvas]
width = 640
height = 360
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "cam_a"
kind = "hls"
url = "https://h.test/a/master.m3u8"
[sources.captions]
mode = "auto"
[[sources]]
id = "cam_b"
kind = "hls"
url = "https://h.test/b/master.m3u8"
[sources.captions]
mode = "auto"
[[sources]]
id = "cam_off"
kind = "hls"
url = "https://h.test/off/master.m3u8"
[sources.captions]
mode = "off"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "cam_b"
"##;
        let config = multiview_config::MultiviewConfig::load_from_toml(toml).expect("parse config");
        let fetcher = CannedFetcher(Ok(MASTER_WITH_SUBS.to_owned()));
        let mut plans = resolve_caption_plans_with(&config.sources, &fetcher);
        plans.sort_by(|a, b| a.id.cmp(&b.id));

        let ids: Vec<&str> = plans.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["cam_a", "cam_b"],
            "both captioned HLS sources resolve; the OFF source does not"
        );
        for plan in &plans {
            assert!(
                plan.rendition_url
                    .ends_with("subtitles/eng/prog_index.m3u8"),
                "rendition_url = {}",
                plan.rendition_url
            );
        }
    }

    /// A hostile text cue over `[start, start + 1000)` ns: 40 padded lines
    /// after a blank one, tagged `c{tag}`.
    #[cfg(feature = "overlay")]
    fn hostile_cue(tag: &str, start: i64) -> CaptionCue {
        let pad = "x".repeat(256 * 1024);
        let mut lines: Vec<String> = (0..40).map(|n| format!("c{tag} l{n} {pad}")).collect();
        lines.insert(0, String::new());
        CaptionCue::try_text(
            MediaTime::from_nanos(start),
            MediaTime::from_nanos(start + 1_000),
            lines,
            None,
        )
        .unwrap()
    }

    /// Issue #430 (MED2-1): overlapping cut cues count once each, however
    /// the newest-active selection alternates between them.
    #[cfg(feature = "overlay")]
    #[test]
    fn cue_store_source_counts_overlapping_cut_cues_once_each() {
        use multiview_overlay::CueSource as _;
        let window = |tag: &str, start: i64, end: i64| {
            let CaptionCue::Text { text, .. } = hostile_cue(tag, start) else {
                panic!("text cue");
            };
            CaptionCue::try_text(
                MediaTime::from_nanos(start),
                MediaTime::from_nanos(end),
                text.lines,
                None,
            )
            .unwrap()
        };
        let probe = |cues: Vec<CaptionCue>, samples: &[i64]| {
            let stats: &'static crate::overlays::CaptionBurnStats = Box::leak(Box::default());
            let store = Arc::new(CueStore::new());
            for cue in cues {
                store.publish(cue.start(), cue.end(), cue);
            }
            let source = CueStoreSource::with_tally(
                store,
                crate::overlays::TruncationTally::with_stats(stats),
            );
            let mut seen = Vec::new();
            for &t in samples {
                let cue = source.active_at(MediaTime::from_nanos(t)).unwrap();
                seen.push(cue.lines.first().unwrap().get(..2).unwrap().to_owned());
            }
            (seen, stats.text_truncated())
        };
        // A = [0, 10), B = [2, 3): samples 1, 2, 4 select A, B, A.
        let (seen, count) = probe(vec![window("A", 0, 10), window("B", 2, 3)], &[1, 2, 4]);
        assert_eq!(seen, ["cA", "cB", "cA"]);
        assert_eq!(count, 2, "A, B, A is two cut cues");
        // An older overlapping cue first surfaces after the newer one ends:
        // A = [0, 10) published, then B = [0, 5) (newest wins until 5).
        let (seen, count) = probe(
            vec![window("A", 0, 10), window("B", 0, 5)],
            &[1, 2, 6, 7, 3, 8],
        );
        assert_eq!(seen, ["cB", "cB", "cA", "cA", "cB", "cA"]);
        assert_eq!(count, 2, "B then the exposed A, each once");
    }

    /// Issue #430: the clock-thread text projection the burn-in path samples
    /// is bounded (inspection + output).
    #[cfg(feature = "overlay")]
    #[test]
    fn cue_store_source_projects_hostile_text_bounded() {
        use multiview_overlay::CueSource as _;
        let store = Arc::new(CueStore::new());
        let cue = hostile_cue("0", 0);
        store.publish(cue.start(), cue.end(), cue);
        let source = CueStoreSource::new(Arc::clone(&store));
        for t in 0..10 {
            let cue = source.active_at(MediaTime::from_nanos(t * 50)).unwrap();
            assert!(cue.lines.len() <= multiview_output::hls::subtitle::MAX_CUE_LINES);
            let bytes = cue.lines.iter().map(|l| l.len() + 1).sum::<usize>() - 1;
            assert!(bytes <= multiview_output::hls::subtitle::MAX_CUE_TEXT_BYTES);
            assert!(cue.lines.iter().all(|l| l.capacity() <= 512));
            assert!(cue.lines.first().unwrap().starts_with("c0 l0 "));
        }
        // A small cue passes through unchanged.
        let small_source = CueStoreSource::new(text_store(0, 1_000, "hello"));
        assert_eq!(
            small_source
                .active_at(MediaTime::from_nanos(10))
                .unwrap()
                .lines,
            vec!["hello".to_owned()]
        );
    }

    /// Issue #430 (MED-1): a cut cue is counted exactly once per cue
    /// identity: a held cue once, a same-window replacement once more, the
    /// same window in a new generation once more, a small cue never.
    #[cfg(feature = "overlay")]
    #[test]
    fn cue_store_source_counts_each_cut_cue_exactly_once() {
        use multiview_overlay::CueSource as _;
        let stats: &'static crate::overlays::CaptionBurnStats = Box::leak(Box::default());
        let store = Arc::new(CueStore::new());
        let source = CueStoreSource::with_tally(
            Arc::clone(&store),
            crate::overlays::TruncationTally::with_stats(stats),
        );
        let hold = |source: &CueStoreSource| {
            for t in 0..20 {
                assert!(source.active_at(MediaTime::from_nanos(t * 40)).is_some());
            }
        };
        let cue = hostile_cue("a", 0);
        store.publish(cue.start(), cue.end(), cue);
        hold(&source);
        assert_eq!(stats.text_truncated(), 1, "a held cue counts once");
        // Same start and end, new content: a replacement is a new cue.
        let cue = hostile_cue("b", 0);
        store.publish(cue.start(), cue.end(), cue);
        hold(&source);
        assert_eq!(stats.text_truncated(), 2, "a same-start replacement counts");
        // A reconnect reuses source-relative timestamps.
        store.begin_generation(2);
        let cue = hostile_cue("a", 0);
        store.publish_for_generation(2, cue.start(), cue.end(), cue);
        hold(&source);
        assert_eq!(stats.text_truncated(), 3, "a new generation counts");
        let small = CaptionCue::try_text(
            MediaTime::from_nanos(2_000),
            MediaTime::from_nanos(3_000),
            vec!["small".to_owned()],
            None,
        )
        .unwrap();
        store.publish_for_generation(2, small.start(), small.end(), small);
        assert!(source.active_at(MediaTime::from_nanos(2_500)).is_some());
        assert_eq!(stats.text_truncated(), 3, "a small cue is not counted");
    }

    fn playlist(
        seqs: std::ops::Range<u64>,
        ended: bool,
    ) -> crate::vtt_rendition::RenditionPlaylist {
        let mut text = format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{}\n", seqs.start);
        for seq in seqs {
            let _ = std::fmt::Write::write_fmt(&mut text, format_args!("#EXTINF:5,\n{seq}.vtt\n"));
        }
        if ended {
            text.push_str("#EXT-X-ENDLIST\n");
        }
        crate::vtt_rendition::RenditionPlaylist::parse(&text).expect("playlist")
    }

    /// #526: a fresh live reader starts a few segments behind the edge, then
    /// reads each new segment once; an origin restart re-joins at the edge.
    #[test]
    fn the_rendition_reader_reads_each_live_segment_once() {
        let live = playlist(100..110, false);
        assert_eq!(first_new_segment(&live, None, true), 7);
        assert_eq!(first_new_segment(&live, Some(108), true), 8);
        assert_eq!(first_new_segment(&live, Some(110), true), 10, "nothing new");
        assert_eq!(
            first_new_segment(&playlist(0..3, false), Some(500), true),
            0
        );
        assert_eq!(
            first_new_segment(&playlist(0..10, true), None, true),
            0,
            "VOD from start"
        );
        assert_eq!(first_new_segment(&live, None, false), 0);
    }

    #[test]
    fn rendition_fetches_keep_to_the_rendition_scheme() {
        assert_eq!(
            rendition_protocols("https://a/b.m3u8"),
            Ok(ALLOWED_PROTOCOLS)
        );
        assert_eq!(rendition_protocols("file:///tmp/x.m3u8"), Ok("file"));
        assert_eq!(rendition_protocols("/tmp/x.m3u8"), Ok("file"));
        assert!(rendition_protocols("concat://a|b").is_err());
    }

    fn vtt_cues(doc: &str) -> Vec<crate::vtt_rendition::VttCue> {
        crate::vtt_rendition::parse_segment(doc).expect("segment")
    }

    fn reader_plan() -> CaptionPlan {
        CaptionPlan {
            http_headers: multiview_ffmpeg::HttpHeaders::default(),
            id: "map".to_owned(),
            rendition_url: String::new(),
            store: Arc::new(CueStore::new()),
            live: true,
            generation: 1,
            displayed_time: Arc::new(multiview_framestore::SwapCell::empty()),
            video_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
        }
    }

    /// Publish a video anchor of the next generation (as the video does).
    fn set_anchor(plan: &CaptionPlan, raw0_s: i64, pts0_s: i64) {
        let generation = plan.video_anchor.load().map_or(0, |a| a.generation) + 1;
        plan.video_anchor
            .store(Some(Arc::new(crate::pipeline::LiveAnchor {
                generation,
                out0: MediaTime::ZERO,
                pts0: MediaTime::from_nanos(pts0_s * 1_000_000_000),
                raw_pts0_ns: Some(raw0_s * 1_000_000_000),
            })));
    }

    fn show_at(plan: &CaptionPlan, pts_s: i64) {
        plan.displayed_time.store(Some(Arc::new(
            multiview_framestore::tile::SourceFrameTime {
                pts: MediaTime::from_nanos(pts_s * 1_000_000_000),
                generation: 1,
                event_clock: None,
            },
        )));
    }

    /// #526: a cue on the MPEG-TS clock maps through the video anchor; with
    /// no live anchor (a file/VOD source) it keeps its written time.
    #[test]
    fn a_rendition_cue_maps_through_the_video_anchor() {
        let cues = vtt_cues(
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:03.000\nx\n",
        );
        let cue = cues.first().expect("cue");
        let written = map_cue(CueAnchor::AsWritten, cue).expect("cue");
        assert_eq!(written.start(), MediaTime::from_nanos(1_000_000_000));
        let anchor = CueAnchor::Raw {
            raw0: 10_000_000_000,
            pts0: MediaTime::from_nanos(500_000_000),
        };
        let mapped = map_cue(anchor, cue).expect("cue");
        assert_eq!(mapped.start(), MediaTime::from_nanos(1_500_000_000));
        assert_eq!(mapped.end(), MediaTime::from_nanos(3_500_000_000));
    }

    fn seg(sequence: u64, discontinuity: u64) -> crate::vtt_rendition::Segment {
        crate::vtt_rendition::RenditionPlaylist::parse(&format!(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity}\n#EXTINF:5,\ns.vtt\n"
        ))
        .and_then(|p| p.segments.into_iter().next())
        .expect("segment")
    }

    /// Map `cues` of `segment` through `clock` (no concurrent video change)
    /// and record the segment; the first cue's mapped start in ms.
    fn read_segment(
        clock: &mut ClockMatch,
        plan: &CaptionPlan,
        segment: &crate::vtt_rendition::Segment,
        cues: &[crate::vtt_rendition::VttCue],
        stop: &AtomicBool,
    ) -> Option<i64> {
        let mapping = clock.segment(plan, segment, cues.first(), stop);
        let SegmentMapping::Map(anchor) = mapping else {
            clock.advance(segment, None);
            return None;
        };
        let mapped: Vec<CaptionCue> = cues.iter().filter_map(|c| map_cue(anchor, c)).collect();
        let latest_start = mapped.iter().map(CaptionCue::start).max();
        let earliest_end = mapped.iter().map(CaptionCue::end).min();
        clock.advance(segment, latest_start.zip(earliest_end));
        mapped.first().map(|c| c.start().as_nanos() / 1_000_000)
    }

    /// Like [`read_segment`], while (after 200 ms, on another thread) the
    /// video plays to `shown_s` and publishes the mapping `raw0_s` = `pts0_s`.
    fn read_while_video_moves(
        clock: &mut ClockMatch,
        plan: &Arc<CaptionPlan>,
        segment: &crate::vtt_rendition::Segment,
        cues: &[crate::vtt_rendition::VttCue],
        (raw0_s, pts0_s, shown_s): (i64, i64, i64),
    ) -> Option<i64> {
        let mover = {
            let plan = Arc::clone(plan);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                set_anchor(&plan, raw0_s, pts0_s);
                show_at(&plan, shown_s);
            })
        };
        let start = read_segment(clock, plan, segment, cues, &AtomicBool::new(false));
        mover.join().expect("mover");
        start
    }

    /// The epoch-1 opening used by the splice tests: mapping raw 30 s = 10 s,
    /// picture at 12 s, segment 100 (5 s) carrying a cue at raw 32 s = 12 s.
    fn opened_epoch(plan: &CaptionPlan) -> ClockMatch {
        set_anchor(plan, 30, 10);
        show_at(plan, 12);
        let mut clock = ClockMatch::default();
        let old = vtt_cues("WEBVTT\n\n00:00:32.000 --> 00:00:33.000\nold side\n");
        let start = read_segment(
            &mut clock,
            plan,
            &seg(100, 1),
            &old,
            &AtomicBool::new(false),
        );
        assert_eq!(start, Some(12_000));
        clock
    }

    /// #526 review r5: the reader runs AHEAD through a cue-free rest of epoch
    /// 1 (segments 101..105, 25 s) into epoch 2, whose reset cue (raw 36 s)
    /// maps under the OLD mapping to 16 s — right at the picture (14 s). It
    /// must not be taken: epoch 2 starts at normalized ~37-40 s, so the reader
    /// waits until the picture is there, then takes the video's (rebased)
    /// mapping raw 35 s = 40 s: 41 s.
    #[test]
    fn a_reader_ahead_of_a_reused_clock_waits_for_the_picture_to_cross() {
        let plan = Arc::new(reader_plan());
        let mut clock = opened_epoch(&plan);
        show_at(&plan, 14);
        let stop = AtomicBool::new(false);
        for sequence in 101..=105 {
            assert_eq!(
                read_segment(&mut clock, &plan, &seg(sequence, 1), &[], &stop),
                None
            );
        }
        let reset = vtt_cues("WEBVTT\n\n00:00:36.000 --> 00:00:37.000\nafter silence\n");
        let start = read_while_video_moves(&mut clock, &plan, &seg(106, 2), &reset, (35, 40, 41));
        assert_eq!(start, Some(41_000), "41 s, not 16 s");
    }

    /// The same splice when the picture is already in epoch 2 (rebase
    /// published): the reset cue maps at once through the new mapping.
    #[test]
    fn a_reset_after_a_caption_silence_takes_the_rebased_mapping() {
        let plan = reader_plan();
        let mut clock = opened_epoch(&plan);
        let stop = AtomicBool::new(false);
        for sequence in 101..=105 {
            read_segment(&mut clock, &plan, &seg(sequence, 1), &[], &stop);
        }
        set_anchor(&plan, 35, 40);
        show_at(&plan, 41);
        let reset = vtt_cues("WEBVTT\n\n00:00:36.000 --> 00:00:37.000\nafter silence\n");
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(106, 2), &reset, &stop),
            Some(41_000)
        );
    }

    /// A short backward reset right after the old cues (raw 33 s -> 1 s) and a
    /// forward jump (-> 61 s): each new epoch takes the mapping the video has
    /// once it crossed, never the old one.
    #[test]
    fn short_resets_and_forward_jumps_take_the_crossed_mapping() {
        for (cue, (raw0, pts0), expect) in [
            ("00:00:01.000", (0, 15), 16_000),
            ("00:01:01.000", (60, 15), 16_000),
        ] {
            let plan = Arc::new(reader_plan());
            let mut clock = opened_epoch(&plan);
            let next = vtt_cues(&format!("WEBVTT\n\n{cue} --> 00:59:00.000\nnew epoch\n"));
            // Epoch 2 starts in [12, 18] s and ends no earlier than 17 s.
            let start =
                read_while_video_moves(&mut clock, &plan, &seg(101, 2), &next, (raw0, pts0, 16));
            assert_eq!(start, Some(expect), "{cue}");
        }
    }

    /// The video republishing the SAME mapping (a new anchor generation)
    /// before it crosses is not a crossing: the reader still waits for the
    /// picture to reach the new epoch.
    #[test]
    fn a_republished_same_mapping_is_not_a_splice_crossing() {
        let plan = Arc::new(reader_plan());
        let mut clock = opened_epoch(&plan);
        set_anchor(&plan, 35, 15);
        show_at(&plan, 13);
        let reset = vtt_cues("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nad\n");
        let start = read_while_video_moves(&mut clock, &plan, &seg(101, 2), &reset, (0, 16, 16));
        assert_eq!(start, Some(17_000), "17 s, not -19 s");
    }

    /// Late chunks of the mapped epoch keep its stored mapping after the
    /// video crossed; an older epoch after a newer one is skipped.
    #[test]
    fn a_late_old_epoch_segment_keeps_its_epoch_mapping() {
        let plan = reader_plan();
        let mut clock = opened_epoch(&plan);
        set_anchor(&plan, 0, 20);
        show_at(&plan, 14);
        let stop = AtomicBool::new(false);
        let late = vtt_cues("WEBVTT\n\n00:00:34.000 --> 00:00:35.000\nlate old side\n");
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(101, 1), &late, &stop),
            Some(14_000),
            "through the old mapping, not 54 s"
        );
        show_at(&plan, 21);
        let new = vtt_cues("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nnew\n");
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(102, 2), &new, &stop),
            Some(21_000)
        );
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(103, 1), &late, &stop),
            None
        );
    }

    /// A picture already past a new epoch's segment skips it (stale), at once.
    #[test]
    fn a_new_epoch_segment_the_picture_passed_is_skipped() {
        let plan = reader_plan();
        let mut clock = opened_epoch(&plan);
        show_at(&plan, 40);
        let started = std::time::Instant::now();
        let next = vtt_cues("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n");
        assert_eq!(
            read_segment(
                &mut clock,
                &plan,
                &seg(101, 2),
                &next,
                &AtomicBool::new(false)
            ),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// #526 review r6/r9: a long cue carried over from segments earlier (it
    /// starts at 90 s, the segment covers 100..105 s) widens the position
    /// estimate. The reader then fails closed on epoch 2 segments whose epoch
    /// the picture's position cannot prove (never a wrong mapping), and
    /// resumes on the first segment it can place: here the 4th (120..125 s).
    #[test]
    fn a_carried_over_cue_fails_closed_then_recovers() {
        let plan = reader_plan();
        set_anchor(&plan, 0, 0);
        show_at(&plan, 102);
        let mut clock = ClockMatch::default();
        let stop = AtomicBool::new(false);
        let carried = vtt_cues("WEBVTT\n\n00:01:30.000 --> 00:01:44.000\nstill on screen\n");
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(100, 1), &carried, &stop),
            Some(90_000)
        );
        // Epoch 2 resets raw 105 -> 90 s; the video crossed (raw 90 s = 105 s).
        set_anchor(&plan, 90, 105);
        show_at(&plan, 106);
        let mut results = Vec::new();
        for (sequence, raw) in [
            (101, "01:30.500"),
            (102, "01:35.500"),
            (103, "01:40.500"),
            (104, "01:45.500"),
        ] {
            let cue = vtt_cues(&format!("WEBVTT\n\n00:{raw} --> 00:59:00.000\nepoch 2\n"));
            results.push(read_segment(
                &mut clock,
                &plan,
                &seg(sequence, 2),
                &cue,
                &stop,
            ));
        }
        assert_eq!(results, vec![None, None, None, Some(120_500)]);
    }

    /// #526 review r8: after the carried-over cue (segment 100..105 s), the
    /// next epoch RESETS the raw clock 105 -> 90 s. Its cue (raw 90.5 s)
    /// projects inside the segment's possible span under the OLD mapping; it
    /// is never mapped there (90.5 s).
    #[test]
    fn a_reset_into_the_possible_span_is_never_mapped_through_the_old_clock() {
        let plan = reader_plan();
        set_anchor(&plan, 0, 0);
        show_at(&plan, 102);
        let mut clock = ClockMatch::default();
        let stop = AtomicBool::new(false);
        let carried = vtt_cues("WEBVTT\n\n00:01:30.000 --> 00:01:44.000\nstill on screen\n");
        read_segment(&mut clock, &plan, &seg(100, 1), &carried, &stop).expect("maps");
        let next = vtt_cues("WEBVTT\n\n00:01:30.500 --> 00:01:32.000\nafter reset\n");
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(101, 2), &next, &stop),
            None
        );
    }

    /// A splice that KEEPS the clock maps through the unchanged mapping once
    /// the picture is surely past the segment start (never through a guess).
    #[test]
    fn a_clock_keeping_splice_maps_once_the_picture_surely_crossed() {
        let plan = Arc::new(reader_plan());
        set_anchor(&plan, 0, 0);
        show_at(&plan, 101);
        let mut clock = ClockMatch::default();
        let stop = AtomicBool::new(false);
        // Segment 100 starts in (98, 101) (cue 103..105.5 starts before its
        // end, 100.5..101 ends after its start): the next in (103, 106).
        let cues = vtt_cues(
            "WEBVTT\n\n00:01:43.000 --> 00:01:45.500\na\n\n00:01:40.500 --> 00:01:41.000\nb\n",
        );
        read_segment(&mut clock, &plan, &seg(100, 1), &cues, &stop).expect("maps");
        let next = vtt_cues("WEBVTT\n\n00:01:46.500 --> 00:01:49.000\nsame clock\n");
        let start = read_while_video_moves(&mut clock, &plan, &seg(101, 2), &next, (0, 0, 107));
        assert_eq!(
            start,
            Some(106_500),
            "while its window (106.5..109 s) is open"
        );
    }

    /// #526 review r9: a LAGGING reader reaches epoch 2's segment (105..110 s
    /// after the carried-over cue) when the picture (111 s) is already in
    /// epoch 3 (anchor raw 0 s = 110 s). It must skip the segment, never map
    /// it through epoch 3's mapping.
    #[test]
    fn a_lagging_reader_never_maps_an_epoch_through_a_later_one() {
        let plan = reader_plan();
        set_anchor(&plan, 0, 0);
        show_at(&plan, 102);
        let mut clock = ClockMatch::default();
        let stop = AtomicBool::new(false);
        let carried = vtt_cues("WEBVTT\n\n00:01:30.000 --> 00:01:44.000\nstill on screen\n");
        read_segment(&mut clock, &plan, &seg(100, 1), &carried, &stop).expect("maps");
        set_anchor(&plan, 0, 110);
        show_at(&plan, 111);
        let epoch2 = vtt_cues("WEBVTT\n\n00:01:30.500 --> 00:01:32.000\nepoch 2\n");
        assert_eq!(
            read_segment(&mut clock, &plan, &seg(101, 2), &epoch2, &stop),
            None
        );
    }

    /// An anchor published ahead of the shown picture (the jitter hold) is
    /// not the picture's mapping: the reader waits for the picture to reach
    /// it.
    #[test]
    fn an_anchor_ahead_of_the_picture_is_not_taken() {
        let plan = Arc::new(reader_plan());
        let mut clock = opened_epoch(&plan);
        show_at(&plan, 15);
        // Rebase published for a picture at 16 s, not yet shown.
        set_anchor(&plan, 0, 16);
        let reset = vtt_cues("WEBVTT\n\n00:00:00.500 --> 00:00:03.000\nad\n");
        let start = read_while_video_moves(&mut clock, &plan, &seg(101, 2), &reset, (0, 16, 16));
        assert_eq!(start, Some(16_500));
    }

    /// #526 review r10: a failed segment fetch keeps the position estimate
    /// (its listed duration advances it), so the following new-epoch segment
    /// still goes through the crossing rules, never the timestamp re-sync.
    #[test]
    fn a_failed_segment_fetch_keeps_the_position_estimate() {
        let plan = reader_plan();
        set_anchor(&plan, 0, 0);
        show_at(&plan, 102);
        let mut clock = ClockMatch::default();
        let stop = AtomicBool::new(false);
        let cue = vtt_cues("WEBVTT\n\n00:01:41.000 --> 00:01:42.000\nepoch 1\n");
        read_segment(&mut clock, &plan, &seg(100, 1), &cue, &stop).expect("maps");
        // Segment 101 fails to fetch: only its duration is recorded.
        clock.advance(&seg(101, 1), None);
        // Segment 102 opens epoch 2 with a reused raw time near the picture:
        // under the old mapping it would land at 90.5 s.
        // It waits for the crossing (stopped here after 300 ms).
        let reset = vtt_cues("WEBVTT\n\n00:01:30.500 --> 00:01:32.000\nepoch 2\n");
        let waiting = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                waiting.store(true, Ordering::Release);
            });
            assert_eq!(
                clock.segment(&plan, &seg(102, 2), reset.first(), &waiting),
                SegmentMapping::Stop,
                "waits for the crossing, never maps through epoch 1's mapping"
            );
        });
    }

    /// #526 review r11: a window slide whose first listed segment FAILS to
    /// fetch is still a gap: the estimate and the epoch mapping are reset.
    #[test]
    fn a_gap_hidden_behind_a_failed_fetch_still_resets() {
        let plan = reader_plan();
        set_anchor(&plan, 0, 0);
        show_at(&plan, 102);
        let mut clock = ClockMatch::default();
        let stop = AtomicBool::new(false);
        let cue = vtt_cues("WEBVTT\n\n00:01:41.000 --> 00:01:42.000\nepoch 1\n");
        read_segment(&mut clock, &plan, &seg(100, 1), &cue, &stop).expect("maps");
        // The window slid to 200; segment 200 fails to fetch.
        clock.advance(&seg(200, 3), None);
        assert_eq!(clock.mapped, None, "epoch mapping reset");
        assert_eq!(clock.next_start, None, "estimate reset");
        assert_eq!(clock.next_seq, Some(201));
    }

    /// The wait ends at once on stop.
    #[test]
    fn a_clock_wait_ends_on_stop() {
        let plan = reader_plan();
        let cues = vtt_cues("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n");
        assert_eq!(
            ClockMatch::default().segment(&plan, &seg(0, 0), cues.first(), &AtomicBool::new(true)),
            SegmentMapping::Stop
        );
    }

    /// #526 review: a network master can never point the reader at a local
    /// file, whatever its subtitle rendition URI says.
    #[test]
    fn a_network_master_cannot_name_a_local_rendition() {
        struct Remote;
        impl PlaylistFetcher for Remote {
            fn fetch(&self, _url: &str) -> Result<FetchedPlaylist, String> {
                Ok(FetchedPlaylist {
                    url: "https://cdn.example/live/master.m3u8".to_owned(),
                    body: "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"s\",NAME=\"en\",\
                           LANGUAGE=\"en\",DEFAULT=YES,URI=\"file:///etc/subs.m3u8\"\n\
                           #EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES=\"s\"\nv.m3u8\n"
                        .to_owned(),
                })
            }
        }
        assert!(
            resolve_caption_plan("cam", "https://cdn.example/live/master.m3u8", None, &Remote)
                .is_none()
        );
        assert!(is_network_url("https://a/b"));
        assert!(!is_network_url("file:///a/b"));
    }

    /// RT-10b: a per-source [`CueStore`] carrying one text cue, for the router
    /// tests. `lines` is non-empty so it renders.
    #[cfg(feature = "overlay")]
    fn text_store(start_ns: i64, end_ns: i64, line: &str) -> Arc<CueStore> {
        let store = Arc::new(CueStore::new());
        let cue = CaptionCue::try_text(
            MediaTime::from_nanos(start_ns),
            MediaTime::from_nanos(end_ns),
            vec![line.to_owned()],
            None,
        )
        .expect("valid text cue");
        store.publish(cue.start(), cue.end(), cue);
        store
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn huge_batches_and_repeated_bursts_keep_earliest_relevant_cue_bounded() {
        let mut pending = PendingCues::default();
        let cursor = MediaTime::from_nanos(1_500_000_000);
        for burst in 0..8 {
            for n in (0..300).rev() {
                let start = MediaTime::from_nanos(i64::from(n) * 1_000_000_000);
                let end = MediaTime::from_nanos(start.as_nanos() + 2_000_000_000);
                let text = format!("cue-{burst}-{n}");
                pending.push(
                    CaptionCue::try_text(start, end, vec![text], None).unwrap(),
                    Some(cursor),
                );
                assert!(pending.cues.len() <= PENDING_CUE_CAP);
                assert!(pending.bytes <= PENDING_BYTES_CAP);
            }
        }
        assert!(pending
            .cues
            .iter()
            .any(|cue| cue.start() <= cursor && cursor < cue.end()));
        assert!(pending.dropped > 0);
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn one_huge_decoded_batch_keeps_the_active_cue_without_retaining_the_batch() {
        use multiview_framestore::tile::SourceFrameTime;
        let store = Arc::new(CueStore::new());
        let cursor = Arc::new(multiview_framestore::SwapCell::new(Some(Arc::new(
            SourceFrameTime {
                pts: MediaTime::from_nanos(1_500_000_000),
                generation: 1,
                event_clock: None,
            },
        ))));
        let plan = CaptionPlan {
            http_headers: multiview_ffmpeg::HttpHeaders::default(),
            id: "huge-batch".to_owned(),
            rendition_url: String::new(),
            store: Arc::clone(&store),
            live: false,
            generation: 1,
            displayed_time: cursor,
            video_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
        };
        let cues: Vec<_> = (0..800)
            .rev()
            .map(|second| {
                let start = MediaTime::from_nanos(i64::from(second) * 1_000_000_000);
                CaptionCue::try_text(
                    start,
                    MediaTime::from_nanos(start.as_nanos() + 2_000_000_000),
                    vec!["payload".repeat(512)],
                    None,
                )
                .unwrap()
            })
            .collect();
        let mut pending = PendingCues::default();
        publish_cues(&plan, &mut pending, cues, &AtomicBool::new(true));
        assert!(
            pending.cues.is_empty(),
            "stop discards untrusted decoder batch"
        );
        let cues: Vec<_> = (0..800)
            .rev()
            .map(|second| {
                let start = MediaTime::from_nanos(i64::from(second) * 1_000_000_000);
                CaptionCue::try_text(
                    start,
                    MediaTime::from_nanos(start.as_nanos() + 2_000_000_000),
                    vec!["payload".repeat(512)],
                    None,
                )
                .unwrap()
            })
            .collect();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| publish_cues(&plan, &mut pending, cues, &stop));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while store
                .active_at(MediaTime::from_nanos(1_500_000_000))
                .is_none()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(store
                .active_at(MediaTime::from_nanos(1_500_000_000))
                .is_some());
            stop.store(true, Ordering::Release);
            worker.join().unwrap();
        });
        assert!(pending.cues.len() <= PENDING_CUE_CAP);
        assert!(pending.bytes <= PENDING_BYTES_CAP);
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn seek_and_new_epoch_refuse_old_pending_cues_and_recover_new_cue() {
        use multiview_framestore::tile::SourceFrameTime;
        let store = Arc::new(CueStore::new());
        let cursor = Arc::new(multiview_framestore::SwapCell::new(Some(Arc::new(
            SourceFrameTime {
                pts: MediaTime::from_nanos(50_000_000_000),
                generation: 1,
                event_clock: None,
            },
        ))));
        let mut pending = PendingCues::default();
        let old = CaptionCue::try_text(
            MediaTime::from_nanos(51_000_000_000),
            MediaTime::from_nanos(53_000_000_000),
            vec!["old".to_owned()],
            None,
        )
        .unwrap();
        pending.push(old, Some(MediaTime::from_nanos(50_000_000_000)));
        store.begin_generation(2);
        cursor.store(Some(Arc::new(SourceFrameTime {
            pts: MediaTime::from_nanos(1_000_000_000),
            generation: 2,
            event_clock: None,
        })));
        let fresh = CaptionCue::try_text(
            MediaTime::ZERO,
            MediaTime::from_nanos(3_000_000_000),
            vec!["fresh".to_owned()],
            None,
        )
        .unwrap();
        let plan = CaptionPlan {
            http_headers: multiview_ffmpeg::HttpHeaders::default(),
            id: "seek".to_owned(),
            rendition_url: String::new(),
            store: Arc::clone(&store),
            live: false,
            generation: 2,
            displayed_time: Arc::clone(&cursor),
            video_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
        };
        let stop = AtomicBool::new(false);
        publish_cues(&plan, &mut PendingCues::default(), vec![fresh], &stop);
        assert!(store
            .active_at_generation(MediaTime::from_nanos(1_000_000_000), 2)
            .is_some());
        assert!(store
            .active_at_generation(MediaTime::from_nanos(51_000_000_000), 1)
            .is_none());
        assert_eq!(
            pending.cues.len(),
            1,
            "old reader pending remains isolated from the new plan"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn buffered_picture_over_twelve_seconds_does_not_evict_its_caption() {
        use multiview_framestore::tile::SourceFrameTime;
        let store = Arc::new(CueStore::new());
        let cursor = Arc::new(multiview_framestore::SwapCell::new(Some(Arc::new(
            SourceFrameTime {
                pts: MediaTime::from_nanos(1_500_000_000),
                generation: 1,
                event_clock: None,
            },
        ))));
        let plan = CaptionPlan {
            http_headers: multiview_ffmpeg::HttpHeaders::default(),
            id: "buffered".to_owned(),
            rendition_url: String::new(),
            store: Arc::clone(&store),
            live: false,
            generation: 1,
            displayed_time: Arc::clone(&cursor),
            video_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let cues = (1..=28)
                .map(|second| {
                    let start = MediaTime::from_nanos(i64::from(second) * 1_000_000_000);
                    let end = MediaTime::from_nanos(start.as_nanos() + 1_000_000_000);
                    CaptionCue::try_text(start, end, vec![format!("cue-{second}")], None).unwrap()
                })
                .collect();
            publish_cues(&plan, &mut PendingCues::default(), cues, &writer_stop);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while store
            .active_at(MediaTime::from_nanos(1_500_000_000))
            .is_none()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(store
            .active_at(MediaTime::from_nanos(1_500_000_000))
            .is_some());
        // Wall time may advance >12 s while the selected picture stays at 1.5 s.
        // Simulate those output ticks without a real sleep: the reader cannot
        // drain future cues into the 16-entry drop-oldest window.
        for _ in 0..350 {
            assert!(store
                .active_at(MediaTime::from_nanos(1_500_000_000))
                .is_some());
        }
        assert!(
            !writer.is_finished(),
            "future caption decode waits for selected picture"
        );
        cursor.store(Some(Arc::new(SourceFrameTime {
            pts: MediaTime::from_nanos(13_500_000_000),
            generation: 1,
            event_clock: None,
        })));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while store
            .active_at(MediaTime::from_nanos(13_500_000_000))
            .is_none()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(store
            .active_at(MediaTime::from_nanos(13_500_000_000))
            .is_some());
        stop.store(true, Ordering::Release);
        writer.join().unwrap();
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn selected_video_time_holds_caption_and_replacement_clears_it() {
        use multiview_framestore::tile::SourceFrameTime;
        let store = text_store(1_000, 5_000, "frame cue");
        let mut router = SubtitleRouter::from_stores([("cam".to_owned(), Arc::clone(&store))]);
        let selected = |pts, generation| {
            std::collections::HashMap::from([(
                "cam".to_owned(),
                SourceFrameTime {
                    pts: MediaTime::from_nanos(pts),
                    generation,
                    event_clock: None,
                },
            )])
        };
        assert_eq!(
            router.sample_selected(&selected(2_000, 1)).get("cam"),
            Some(&vec!["frame cue".to_owned()])
        );
        // Output ticks advance, but the selected picture does not.
        assert_eq!(
            router.sample_selected(&selected(2_000, 1)).get("cam"),
            Some(&vec!["frame cue".to_owned()])
        );
        assert!(router.sample_selected(&selected(5_000, 1)).is_empty());
        store.begin_generation(2);
        assert!(router.sample_selected(&selected(2_000, 1)).is_empty());
        assert!(router.sample_selected(&selected(2_000, 2)).is_empty());
        let stale = CaptionCue::try_text(
            MediaTime::from_nanos(1_000),
            MediaTime::from_nanos(5_000),
            vec!["stale".to_owned()],
            None,
        )
        .unwrap();
        store.publish_for_generation(1, stale.start(), stale.end(), stale);
        assert!(router.sample_selected(&selected(2_000, 2)).is_empty());
    }

    /// RT-10b: with one layer per source bound to its OWN store and no re-point,
    /// the router's per-tick `sample` yields exactly the active text lines the
    /// old per-source `active_at` sampling did — identical behaviour.
    #[cfg(feature = "overlay")]
    #[test]
    fn router_sample_matches_per_source_text_when_not_repointed() {
        let a = text_store(1_000, 5_000, "alpha");
        let b = text_store(2_000, 6_000, "bravo");
        let mut router = SubtitleRouter::from_stores([
            ("cam_a".to_owned(), Arc::clone(&a)),
            ("cam_b".to_owned(), Arc::clone(&b)),
        ]);

        // Inside both windows: each layer shows its own source's lines.
        let at = router.sample(MediaTime::from_nanos(3_000));
        assert_eq!(at.get("cam_a"), Some(&vec!["alpha".to_owned()]));
        assert_eq!(at.get("cam_b"), Some(&vec!["bravo".to_owned()]));

        // Before cam_b's window: only cam_a is active (a source with no active cue
        // is absent, exactly like `sample_caption_stores`).
        let early = router.sample(MediaTime::from_nanos(1_500));
        assert_eq!(early.get("cam_a"), Some(&vec!["alpha".to_owned()]));
        assert_eq!(early.get("cam_b"), None);

        // Outside all windows: nothing renders.
        let none = router.sample(MediaTime::from_nanos(9_000));
        assert!(none.is_empty());
    }

    /// RT-10b: re-pointing the `cam_a` layer to `cam_b`'s store makes the layer
    /// render `cam_b`'s cues on the NEXT sample — the subtitle breakaway is
    /// effective through the run's sampling path. The seam clears the old cue
    /// (CLEAR-on-switch) so no stale `cam_a` cue flashes at the boundary.
    #[cfg(feature = "overlay")]
    #[test]
    fn repointing_a_layer_renders_the_new_source_on_the_next_sample() {
        let a = text_store(0, 100_000, "alpha-wide");
        let b = text_store(2_000, 6_000, "bravo");
        let mut router = SubtitleRouter::from_stores([
            ("cam_a".to_owned(), Arc::clone(&a)),
            ("cam_b".to_owned(), Arc::clone(&b)),
        ]);

        // Steady: cam_a shows its wide cue.
        let before = router.sample(MediaTime::from_nanos(1_000));
        assert_eq!(before.get("cam_a"), Some(&vec!["alpha-wide".to_owned()]));

        // BREAKAWAY: route the cam_a layer to cam_b's cue source.
        router.repoint("cam_a", "cam_b");

        // Seam at t=1500 (inside cam_a's wide cue, before cam_b's window): the old
        // wide cam_a cue must NOT flash — the layer now reads cam_b (no cue yet) and
        // clears. cam_b's own layer is unaffected (also no cue yet here).
        let seam = router.sample(MediaTime::from_nanos(1_500));
        assert_eq!(
            seam.get("cam_a"),
            None,
            "CLEAR-on-switch: the stale cam_a cue must not flash at the seam"
        );

        // Past the seam, inside cam_b's window: the cam_a layer now renders cam_b's
        // cue — the breakaway is live.
        let after = router.sample(MediaTime::from_nanos(3_000));
        assert_eq!(
            after.get("cam_a"),
            Some(&vec!["bravo".to_owned()]),
            "after the breakaway the cam_a layer must render cam_b's cues"
        );
        // cam_b's own layer still renders cam_b too (independent layers).
        assert_eq!(after.get("cam_b"), Some(&vec!["bravo".to_owned()]));
    }

    /// RT-10b: a re-point requested OFF-THREAD through the wait-free
    /// [`SubtitleRouteHandle`] (the `RouteSubtitle` seam) is applied at the next
    /// `sample` boundary — so the run's sampling path honours a control-plane
    /// breakaway without the control plane ever touching the hot-loop-owned router.
    #[cfg(feature = "overlay")]
    #[test]
    fn handle_request_repoint_takes_effect_on_the_next_sample() {
        let a = text_store(0, 100_000, "alpha-wide");
        let b = text_store(2_000, 6_000, "bravo");
        let mut router = SubtitleRouter::from_stores([
            ("cam_a".to_owned(), Arc::clone(&a)),
            ("cam_b".to_owned(), Arc::clone(&b)),
        ]);
        let handle = router.handle();

        // Steady: cam_a shows its own cue.
        assert_eq!(
            router.sample(MediaTime::from_nanos(1_000)).get("cam_a"),
            Some(&vec!["alpha-wide".to_owned()])
        );

        // The control plane (here, this thread via the shared handle) requests the
        // breakaway. The router has NOT been told directly.
        handle.request_repoint("cam_a", "cam_b");

        // The very next sample drains + applies the pending request: the seam clears
        // the stale cam_a cue (CLEAR-on-switch), then cam_a's layer reads cam_b.
        let seam = router.sample(MediaTime::from_nanos(1_500));
        assert_eq!(
            seam.get("cam_a"),
            None,
            "the pending handle re-point must apply at the next sample (seam clears)"
        );
        let after = router.sample(MediaTime::from_nanos(3_000));
        assert_eq!(
            after.get("cam_a"),
            Some(&vec!["bravo".to_owned()]),
            "after the handle re-point the cam_a layer must render cam_b's cues"
        );
    }

    /// RT-10b: the wait-free pending slot is bounded drop-oldest (safety rule §5) —
    /// a storm of re-point requests never grows unbounded, and the NEWEST binding
    /// for a layer is the one that takes effect.
    #[cfg(feature = "overlay")]
    #[test]
    fn handle_repoint_backlog_is_bounded_and_newest_wins() {
        let a = text_store(0, 100_000, "alpha");
        let b = text_store(0, 100_000, "bravo");
        let c = text_store(0, 100_000, "charlie");
        let mut router = SubtitleRouter::from_stores([
            ("cam_a".to_owned(), Arc::clone(&a)),
            ("cam_b".to_owned(), Arc::clone(&b)),
            ("cam_c".to_owned(), Arc::clone(&c)),
        ]);
        let handle = router.handle();

        // Storm far past the backlog cap, ending on the binding the operator wants.
        for _ in 0..(MAX_SUBTITLE_REPOINT_BACKLOG * 3) {
            handle.request_repoint("cam_a", "cam_b");
        }
        handle.request_repoint("cam_a", "cam_c"); // the last request wins

        let after = router.sample(MediaTime::from_nanos(1_000));
        assert_eq!(
            after.get("cam_a"),
            Some(&vec!["charlie".to_owned()]),
            "the newest re-point (cam_c) must be the one in effect after the storm"
        );
    }

    /// A one-shot `[::1]` HTTP server that records the request head it receives
    /// and answers `body` as a playlist.
    fn serve_once(body: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("[::1]:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0_u8; 4096];
                let n = conn.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{addr}/master.m3u8"), rx)
    }

    #[test]
    fn caption_master_fetch_and_plan_carry_the_operator_headers() {
        let (url, rx) = serve_once(MASTER_WITH_SUBS);
        let source = hls_source_for_test(&url, Some("Opera/Test 9"), Some("https://ref.example/"));
        let plan = caption_plan_for(&source).expect("caption plan");
        let head = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("request seen")
            .to_ascii_lowercase();
        assert!(head.contains("user-agent: opera/test 9"), "{head}");
        assert!(head.contains("referer: https://ref.example/"), "{head}");
        assert_eq!(plan.http_headers.user_agent(), Some("Opera/Test 9"));
        assert_eq!(plan.http_headers.referer(), Some("https://ref.example/"));
    }

    #[test]
    fn caption_plan_without_headers_has_an_empty_header_set() {
        let source = hls_source_for_test("https://x.test/m.m3u8", None, None);
        let plan = caption_plan_for_with(&source, &CannedFetcher(Ok(MASTER_WITH_SUBS.to_owned())))
            .expect("plan");
        assert!(plan.http_headers.is_empty());
    }

    fn hls_source_for_test(
        url: &str,
        ua: Option<&str>,
        referrer: Option<&str>,
    ) -> multiview_config::Source {
        let mut source: multiview_config::Source = serde_json::from_value(serde_json::json!({
            "id": "h", "kind": "hls", "url": url, "captions": {"mode": "auto"},
        }))
        .expect("source");
        if let multiview_config::SourceKind::Hls {
            user_agent,
            referrer: r,
            ..
        } = &mut source.kind
        {
            *user_agent = ua.map(str::to_owned);
            *r = referrer.map(str::to_owned);
        }
        source
    }
}
