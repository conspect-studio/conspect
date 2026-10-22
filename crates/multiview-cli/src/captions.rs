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
//! 5. spawns a **second isolated demux** of that rendition `m3u8` on its own
//!    ingest thread that decodes each `WebVTT` cue with
//!    [`multiview_ffmpeg::CaptionDecoder`] and publishes it into a per-source
//!    [`CaptionCueStore`].
//!
//! The reader is **best-effort**: any fetch/parse/decode error logs and yields no
//! cues (the tile simply shows none). It only ever *writes* the lock-free cue
//! store — it can neither pace nor stall the output clock (invariant #1) nor
//! back-pressure the engine (invariant #10), exactly like the video ingest
//! threads. The off-hot-path overlay baker *samples* the store at each output
//! tick (`active_at(pts)`), mirroring frame-store latch-on-tick.
//!
//! ## Timing (invariant #3)
//!
//! libav applies the rendition's `X-TIMESTAMP-MAP` itself, so the `WebVTT`
//! packets arrive already rebased onto a 0-based media timeline (the first cue's
//! packet PTS is `1.0s`, matching the bipbop video's source-relative timeline).
//! The reader anchors each cue's window with [`multiview_ffmpeg::CaptionDecoder`]
//! (packet PTS rebased to ns through the stream time-base) and publishes it on
//! the same source-relative timeline the video frames use, so a cue burns in at
//! the same media instant the output clock samples it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ffmpeg_next as ffmpeg;

use multiview_config::schema::CaptionSelector;
use multiview_core::time::MediaTime;
use multiview_ffmpeg::caption::CaptionCue;
use multiview_ffmpeg::{CaptionDecoder, CaptionSource};
use multiview_input::caption_store::CaptionCueStore;
use multiview_input::hls::MasterPlaylist;

/// A per-source caption cue store: `CaptionCueStore` instantiated for the unified
/// [`CaptionCue`] model. The reader thread writes it; the overlay baker samples it.
pub type CueStore = CaptionCueStore<CaptionCue>;

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
/// `active_at(now)` is the same lock-free [`CueStore::active_at`] read the baker
/// already did, so routing through the layer adds no new hot-path cost and can
/// neither pace nor stall the engine (invariants #1/#10).
#[cfg(feature = "overlay")]
#[derive(Clone)]
pub struct CueStoreSource {
    store: Arc<CueStore>,
}

#[cfg(feature = "overlay")]
impl CueStoreSource {
    /// Wrap a per-source cue store as a routable text [`CueSource`](multiview_overlay::CueSource).
    #[must_use]
    pub fn new(store: Arc<CueStore>) -> Self {
        Self { store }
    }
}

#[cfg(feature = "overlay")]
impl multiview_overlay::CueSource for CueStoreSource {
    fn active_at(&self, now: MediaTime) -> Option<multiview_overlay::subtitle::Cue> {
        match self.store.active_at(now) {
            Some(CaptionCue::Text { start, end, text }) if !text.lines.is_empty() => {
                Some(multiview_overlay::subtitle::Cue {
                    start,
                    end,
                    lines: text.lines,
                })
            }
            // A bitmap cue (or an empty-line text cue) is not a text cue: the
            // text-layer renders nothing for it (the bitmap path is separate).
            _ => None,
        }
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
/// request onto a lock-free RCU slot (the same `arc-swap` read-copy-update the cue
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
    pending: Arc<arc_swap::ArcSwap<Vec<SubtitleRePoint>>>,
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
    /// One re-pointable subtitle layer per source-bound caption store, keyed by the
    /// source id it renders into (the tile target). Sampled (`&mut`) each tick.
    layers: std::collections::HashMap<String, multiview_overlay::SubtitleLayer>,
    /// The per-source cue stores, so a re-point can resolve a target source id to
    /// its [`CueSource`]. Shared by `Arc` with the readers; a lock-free read only.
    stores: std::collections::HashMap<String, Arc<CueStore>>,
    /// The wait-free pending re-point slot the [`SubtitleRouteHandle`] publishes to
    /// and `sample` drains. Shared by `Arc` with every handle clone.
    pending: Arc<arc_swap::ArcSwap<Vec<SubtitleRePoint>>>,
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
        Self {
            layers,
            stores,
            pending: Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new())),
        }
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
        // Sample-boundary apply: take the pending re-points (publish an empty vec in
        // their place) and apply each. Re-pointing is `&self` on the layer, so this
        // is sound while we hold `&mut self`. Empty (the common case) is a cheap
        // pointer load + `is_empty` check — no allocation, no work.
        let pending = self.pending.swap(Arc::new(Vec::new()));
        for rp in pending.iter() {
            self.repoint(&rp.layer_id, &rp.source_id);
        }
        let mut out = std::collections::HashMap::new();
        for (id, layer) in &mut self.layers {
            if let Some(cue) = layer.sample(now) {
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
    pub fn repoint(&self, layer_id: &str, source_id: &str) {
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
    }
}

/// A resolved per-source caption reader plan: which rendition `m3u8` to demux and
/// the store its decoded cues are published into.
pub struct CaptionPlan {
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
    if let Some(abs_path) = rendition_uri.strip_prefix('/') {
        return match origin {
            Some(origin) => format!("{origin}/{abs_path}"),
            // No origin (a bare local master): treat as a filesystem-absolute path.
            None => format!("/{abs_path}"),
        };
    }

    // Directory of the master path: everything up to and including the last '/'.
    let dir = match path.rfind('/') {
        Some(idx) => path.get(..=idx).unwrap_or(""),
        None => "",
    };
    let joined = format!("{dir}{rendition_uri}");
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
    for cue in cues {
        let (start, end) = (cue.start(), cue.end());
        store.publish(start, end, cue);
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
    caption_plan_for_with(source, &LibavFetcher)
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
    resolve_caption_plan(&source.id, master_url, language.as_deref(), fetcher)
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
    resolve_caption_plans_with(sources, &LibavFetcher)
}

/// [`resolve_caption_plans`] with an injectable, thread-shareable fetcher — the
/// concurrent fetch seam, exercised offline in tests with canned bytes (no
/// network, no FFI).
#[must_use]
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
    tracing::info!(
        source = %id,
        language = ?rendition.language,
        %rendition_url,
        "native HLS WebVTT caption rendition resolved"
    );
    Some(CaptionPlan {
        id: id.to_owned(),
        rendition_url,
        store: Arc::new(CueStore::new()),
        // This planner is only reached for an HLS source (via `webvtt_language` →
        // `hls_url`); HLS is a live source (mirrors the video `IngestPlan` deriving
        // `live = true` for `SourceKind::Hls`), so its caption rendition is
        // supervised-reconnected.
        live: true,
    })
}

/// The HLS master URL for a source kind, if it is an HLS source.
fn hls_url(kind: &multiview_config::SourceKind) -> Option<&str> {
    match kind {
        multiview_config::SourceKind::Hls { url } => Some(url.as_str()),
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
        if stop.load(Ordering::Acquire) {
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

/// Open `plan.rendition_url`, decode its `WebVTT` subtitle stream, and publish
/// each cue into `plan.store`. Returns `Ok(())` at clean EOF.
///
/// libav's HLS demuxer is strict about segment extensions; the rendition's
/// `.webvtt` segments mismatch the default allow-list, so `extension_picky` is
/// disabled for this isolated demux (it never touches the program path). No FFI,
/// no `unsafe`: only `ffmpeg-next`'s safe `Input`/`Parameters` value API bridges
/// into `multiview-ffmpeg`'s safe `CaptionDecoder`.
fn read_captions(plan: &CaptionPlan, stop: &AtomicBool) -> Result<(), String> {
    multiview_ffmpeg::ensure_initialized().map_err(|e| e.to_string())?;

    // The HLS demuxer rejects `.webvtt` segments under its default extension
    // allow-list; relax it for THIS isolated rendition demux only. Cap the
    // stream-probe so `avformat_find_stream_info` does NOT read the entire VOD
    // rendition (all 60 segments) up front — a tiny probe is enough to identify
    // the single WebVTT subtitle stream, and decoding then streams the cues out
    // promptly rather than after a multi-second whole-playlist read-ahead.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("extension_picky", "0");
    opts.set("allowed_extensions", "ALL");
    opts.set("probesize", "131072");
    opts.set("analyzeduration", "0");

    let mut input = ffmpeg::format::input_with_dictionary(&plan.rendition_url.as_str(), opts)
        .map_err(|e| e.to_string())?;

    let (stream_index, params, time_base) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Subtitle)
            .ok_or_else(|| "rendition has no subtitle stream".to_owned())?;
        (
            stream.index(),
            stream.parameters(),
            multiview_ffmpeg::from_ff_rational(stream.time_base()),
        )
    };

    let mut decoder = CaptionDecoder::from_parameters(CaptionSource::WebVtt, params, time_base)
        .map_err(|e| e.to_string())?;

    // Pace each decoded cue's publish to wall-clock by its start PTS (invariant
    // #4 — a VOD-as-live rendition is paced by PTS, never slurped). Without this
    // the unpaced reader would drain the whole VOD into the bounded drop-oldest
    // store in seconds, evicting the early cues before the output clock samples
    // them; pacing keeps the store window aligned with output media time so the
    // baker's per-tick `active_at(pts)` lands on the right cue. The pacer only
    // ever *delays the writer*; it never blocks the reader of the store, so it
    // cannot pace or stall the output clock (#1/#10).
    let mut pacer = CuePacer::new();

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {
                if packet.stream() != stream_index {
                    continue;
                }
                // A decode error on one packet is logged and skipped — captions
                // are intermittent and a malformed cue must never stall ingest.
                match decoder.decode(&packet) {
                    Ok(cues) => publish_cues(plan, &mut pacer, cues, stop),
                    Err(e) => {
                        tracing::debug!(source = %plan.id, error = %e, "caption packet decode error");
                    }
                }
            }
            Err(ffmpeg::Error::Eof) => return Ok(()),
            Err(other) => return Err(other.to_string()),
        }
    }
}

/// Publish each decoded cue into the store using its own `[start, end)` window,
/// paced to wall-clock by the cue's start PTS so the bounded store window tracks
/// output media time.
fn publish_cues(
    plan: &CaptionPlan,
    pacer: &mut CuePacer,
    cues: Vec<CaptionCue>,
    stop: &AtomicBool,
) {
    for cue in cues {
        let (start, end) = (cue.start(), cue.end());
        pacer.wait_for(start, stop);
        if stop.load(Ordering::Acquire) {
            return;
        }
        plan.store.publish(start, end, cue);
    }
}

/// A wall-clock pacer keyed on a cue's start PTS (invariant #4): the first cue
/// anchors `base_instant = now` to `base_pts = start`; each later cue is held
/// until `now - base_instant >= start - base_pts`. A backwards PTS re-anchors so
/// a discontinuity never stalls the reader for long.
struct CuePacer {
    anchor: Option<(std::time::Instant, MediaTime)>,
}

impl CuePacer {
    fn new() -> Self {
        Self { anchor: None }
    }

    /// Block (in `stop`-checked slices) until wall-clock reaches `pts`'s release
    /// instant. The first call anchors the timeline and returns immediately.
    fn wait_for(&mut self, pts: MediaTime, stop: &AtomicBool) {
        let Some((base_instant, base_pts)) = self.anchor else {
            self.anchor = Some((std::time::Instant::now(), pts));
            return;
        };
        if pts < base_pts {
            self.anchor = Some((std::time::Instant::now(), pts));
            return;
        }
        let delta = pts.saturating_sub(base_pts);
        let target_ns = u64::try_from(delta.as_nanos()).unwrap_or(0);
        let target = base_instant + std::time::Duration::from_nanos(target_ns);
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = std::time::Instant::now();
            if now >= target {
                return;
            }
            let remaining = target.saturating_duration_since(now);
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
        }
    }
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
        assert!(out.is_empty());
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
}
