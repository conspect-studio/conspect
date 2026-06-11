//! RT-11 / ADR-0034 — **engine apply** of per-stream route intents.
//!
//! The control plane submits `Command::RouteVideo` / `RouteAudio` /
//! `RouteSubtitle` onto the bounded, non-back-pressuring command bus; the engine
//! drains that bus at the frame boundary (between [`clock.tick()`] and
//! [`drive.compose()`], `runtime.rs`). This module is what the drain *does* with
//! a drained batch of routes: it resolves each [`StreamRef`]'s
//! [`StreamSelector`](multiview_config::routing::StreamSelector) against the
//! input's [`StreamInventory`] to the concrete stream, then re-points the live
//! crosspoint by calling the existing O(1) primitives —
//!
//! * **video** → [`CompositorDrive::rebind_cell`] (RT-6),
//! * **audio** → [`ProgramBus::repoint_crossfade`] (RT-8a / RT-9, cross-fade by
//!   default for a pop-free breakaway),
//! * **subtitle** → [`SubtitleLayer::repoint`] (RT-10a).
//!
//! Because `multiview-control` depends on `multiview-engine` (not the reverse),
//! the engine cannot name the control plane's `Command`. So the apply surface is
//! defined here in **engine-native** terms ([`RouteIntent`]); the control plane
//! *desugars* its `Command::Route*` into these intents before submitting, and the
//! engine drives [`RouteApplier`] over the drained batch. `SwapSource` desugars to
//! a `RouteIntent::Video { …, Video, Best }` exactly like the brief's alias.
//!
//! ## Isolation (invariants #1 + #10)
//!
//! Every apply here is an O(1) map/pointer mutation: `rebind_cell` mutates a
//! cell→source binding, `repoint_crossfade`/`repoint` swaps a channel's
//! `Arc<AudioStore>`, `SubtitleLayer::repoint` stores an `Arc<dyn CueSource>`.
//! None decodes, does I/O, blocks, or `.await`s the new source — they are safe on
//! the frame-boundary control hook, which never stalls the output clock. A
//! drained batch is **coalesced** (the last route per destination wins) so a
//! salvo storm of K commands onto one cell is **one** effective re-point per tick,
//! never K (RT-6 hard gate #1).

use std::collections::HashMap;
use std::sync::Arc;

use multiview_audio::{AudioStore, ProgramBus, RoutePoint, SwitchTier};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::routing::{StreamRef, StreamSelector};
use multiview_core::stream::{StreamDescriptor, StreamInventory, StreamKind};
use multiview_overlay::{CueSource, SubtitleLayer};

use crate::drive::CompositorDrive;
use crate::error::{Error, Result};

/// One engine-native route intent — the desugared form of a control-plane
/// `Command::Route*` (or `SwapSource`), applied at the frame boundary.
///
/// `#[non_exhaustive]` so additional crosspoint kinds (output ← program, RT-12)
/// can be added without a breaking change.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RouteIntent {
    /// Re-point a layout cell to a video [`StreamRef`] (RT-6).
    Video {
        /// The destination layout cell id.
        cell: String,
        /// The source elementary stream feeding the cell.
        source: StreamRef,
    },
    /// Re-point a program-bus channel / discrete track to an audio [`StreamRef`]
    /// (RT-8a / RT-9).
    Audio {
        /// The destination bus channel / discrete-track name.
        target: String,
        /// The source elementary stream feeding the target.
        source: StreamRef,
        /// Program-bus contribution gain in dB (`0.0` ⇒ unity).
        gain_db: f32,
        /// Whether the source contributes silence (still routed).
        mute: bool,
    },
    /// Re-point a subtitle layer to a subtitle [`StreamRef`] (RT-10a).
    Subtitle {
        /// The destination subtitle layer id.
        layer: String,
        /// The source elementary stream feeding the layer.
        source: StreamRef,
    },
}

impl RouteIntent {
    /// Build the `RouteIntent::Video` that a legacy `SwapSource{tile,source}`
    /// desugars to — the brief's `RouteVideo{cell, StreamRef{source, Video, Best}}`
    /// alias (back-compat).
    #[must_use]
    pub fn swap_source(tile: impl Into<String>, source: impl Into<String>) -> Self {
        Self::Video {
            cell: tile.into(),
            source: StreamRef::best(source, StreamKind::Video),
        }
    }
}

/// Resolve a [`StreamSelector`] against an input's [`StreamInventory`] to the
/// concrete [`StreamDescriptor`] of the requested `kind`, or [`None`] when no
/// stream matches (deferred-resolution honesty — a `Language`/`Index` that does
/// not resolve is not an error, the stream may appear after a reconnect).
///
/// * [`StreamSelector::Best`] → the inventory's default for the kind family
///   (the `default`-flagged stream, else the first of that kind).
/// * [`StreamSelector::Index`] → the stream at that 0-based position **within
///   the kind**.
/// * [`StreamSelector::Language`] → the first stream of the kind whose validated
///   BCP-47 language tag matches (case-insensitively, by primary subtag).
/// * [`StreamSelector::StreamId`] → the stream whose [`StableStreamId`]'s display
///   form (`scope/key`) equals the requested id.
///
/// [`StableStreamId`]: multiview_core::stream::StableStreamId
#[must_use]
pub fn resolve_selector<'a>(
    inventory: &'a StreamInventory,
    kind: StreamKind,
    selector: &StreamSelector,
) -> Option<&'a StreamDescriptor> {
    let of_kind = move || inventory.streams.iter().filter(move |s| s.kind == kind);
    match selector {
        StreamSelector::Best => inventory.default_for(|k| k == kind),
        StreamSelector::Index { index } => of_kind().nth(*index),
        StreamSelector::Language { language } => of_kind().find(|s| {
            s.language
                .as_ref()
                .is_some_and(|tag| language_matches(tag.as_str(), language))
        }),
        StreamSelector::StreamId { id } => of_kind().find(|s| s.id.to_string() == *id),
        // The selector enum is `#[non_exhaustive]`; an unrecognised future
        // variant resolves to nothing rather than mis-routing.
        _ => None,
    }
}

/// Whether two language tags match for selector resolution — equal ignoring
/// case, or sharing the same primary subtag (so `"eng"` matches `"en"` /
/// `"en-US"`, the common operator intent).
fn language_matches(have: &str, want: &str) -> bool {
    if have.eq_ignore_ascii_case(want) {
        return true;
    }
    let primary = |t: &str| t.split(['-', '_']).next().unwrap_or(t).to_ascii_lowercase();
    let (h, w) = (primary(have), primary(want));
    // ISO-639-2/B vs -1 (3-letter vs 2-letter) common pairs share a prefix only
    // for some languages, so also accept a 2↔3 letter prefix relationship.
    h == w || h.starts_with(&w) || w.starts_with(&h)
}

/// The pre-resolved binding context a [`RouteApplier`] consults to turn a
/// [`StreamRef`] into a concrete live handle.
///
/// Resolution (selector → concrete stream → live handle) is done **off the
/// engine thread** (control-side, at ARM time) and handed to the engine as this
/// table, so the frame-boundary apply is a pure O(1) map lookup + pointer swap
/// (it never probes, allocates a decoder, or blocks — invariants #1/#10). Each
/// handle is keyed by the `StreamRef` the intent carries:
///
/// * **video** → the `CompositorDrive` store key (a registered, decoding source);
/// * **audio** → the source's `Arc<AudioStore>` (warm, ready to read);
/// * **subtitle** → the source's `Arc<dyn CueSource>`.
///
/// The audio `channels` map names the bus [`RoutePoint`] each target name binds.
#[derive(Clone, Default)]
pub struct RouteResolution {
    inventories: HashMap<String, StreamInventory>,
    video_store_keys: HashMap<String, String>,
    audio_stores: HashMap<String, Arc<AudioStore>>,
    cue_sources: HashMap<String, Arc<dyn CueSource>>,
    audio_channels: HashMap<String, RoutePoint>,
}

/// A deterministic, hashable map key for a [`StreamRef`] (it is `Eq` but not
/// `Hash`, and `multiview-config` is read-only here). The key is total and
/// collision-free: `input_id` is length-prefixed so it cannot bleed into the
/// kind/selector, the kind is its display char, and the selector is its
/// distinguishing payload.
fn ref_key(source: &StreamRef) -> String {
    let selector = match &source.selector {
        StreamSelector::Best => "best".to_owned(),
        StreamSelector::Index { index } => format!("idx:{index}"),
        StreamSelector::Language { language } => format!("lang:{language}"),
        StreamSelector::StreamId { id } => format!("id:{id}"),
        // `#[non_exhaustive]`: an unknown future selector gets a stable,
        // distinct key rather than colliding with a known one.
        other => format!("other:{other:?}"),
    };
    format!(
        "{}:{}|{}|{}",
        source.input_id.len(),
        source.input_id,
        kind_tag(source.kind),
        selector
    )
}

/// A short, stable discriminant for a [`StreamKind`] used only in [`ref_key`]
/// (the core `scope_char` is crate-private). Distinct per family + payload.
fn kind_tag(kind: StreamKind) -> String {
    match kind {
        StreamKind::Video => "v".to_owned(),
        StreamKind::Audio => "a".to_owned(),
        StreamKind::Subtitle => "s".to_owned(),
        StreamKind::Data(d) => format!("d:{d:?}"),
        StreamKind::Timecode(t) => format!("t:{t:?}"),
        // `#[non_exhaustive]`: an unknown future kind gets a stable distinct tag.
        other => format!("x:{other:?}"),
    }
}

impl std::fmt::Debug for RouteResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteResolution")
            .field("inventories", &self.inventories.len())
            .field("video_store_keys", &self.video_store_keys.len())
            .field("audio_stores", &self.audio_stores.len())
            .field("cue_sources", &self.cue_sources.len())
            .field("audio_channels", &self.audio_channels.len())
            .finish()
    }
}

impl RouteResolution {
    /// A resolution context carrying the per-input inventories (for selector
    /// validation) and no resolved handles yet.
    #[must_use]
    pub fn new(inventories: HashMap<String, StreamInventory>) -> Self {
        Self {
            inventories,
            ..Self::default()
        }
    }

    /// The inventory of input `input_id`, if known (the selector-validation
    /// surface).
    #[must_use]
    pub fn inventory(&self, input_id: &str) -> Option<&StreamInventory> {
        self.inventories.get(input_id)
    }

    /// Bind a video `StreamRef` to its `CompositorDrive` store key.
    pub fn set_video_store_key(&mut self, source: &StreamRef, key: impl Into<String>) {
        self.video_store_keys.insert(ref_key(source), key.into());
    }

    /// Bind an audio `StreamRef` to its warm `Arc<AudioStore>`.
    pub fn set_audio_store(&mut self, source: &StreamRef, store: Arc<AudioStore>) {
        self.audio_stores.insert(ref_key(source), store);
    }

    /// Bind a subtitle `StreamRef` to its `Arc<dyn CueSource>`.
    pub fn set_cue_source(&mut self, source: &StreamRef, cue: Arc<dyn CueSource>) {
        self.cue_sources.insert(ref_key(source), cue);
    }

    /// Bind a program-bus / discrete-track target name to its [`RoutePoint`].
    pub fn set_audio_channel(&mut self, target: impl Into<String>, point: RoutePoint) {
        self.audio_channels.insert(target.into(), point);
    }

    /// Confirm the `StreamRef` resolves to a concrete stream of its kind in the
    /// input's inventory (when the inventory is known). Returns the resolved
    /// descriptor's display id, or [`None`] when the selector does not resolve.
    fn validate_ref(&self, source: &StreamRef) -> Option<String> {
        let inventory = self.inventories.get(&source.input_id)?;
        resolve_selector(inventory, source.kind, &source.selector).map(|d| d.id.to_string())
    }
}

/// Drives a drained batch of [`RouteIntent`]s onto the live crosspoints at the
/// frame boundary, over a borrowed [`RouteResolution`].
///
/// Constructed once per drain (cheap — it borrows the resolution table). Each
/// `apply_*` coalesces the batch (last route per destination wins) and applies
/// the surviving route O(1). Returns the number of **effective** re-points so a
/// caller can assert coalescing.
#[derive(Debug)]
pub struct RouteApplier<'a> {
    resolution: &'a RouteResolution,
}

impl<'a> RouteApplier<'a> {
    /// Build an applier over a resolution context.
    #[must_use]
    pub fn new(resolution: &'a RouteResolution) -> Self {
        Self { resolution }
    }

    /// Apply the **video** routes in `batch` onto `drive`, coalesced to one
    /// effective re-point per cell (the last route per cell wins — a salvo storm
    /// of K commands onto one cell is one re-point, RT-6 hard gate #1). Returns
    /// the number of cells actually re-pointed.
    ///
    /// # Errors
    ///
    /// [`Error::Route`] when a route's `StreamRef` does not resolve in the
    /// (known) inventory, or [`Error::Rebind`] when the destination cell / target
    /// store is not addressable. The binding is held unchanged on error — never a
    /// panic, never a silent mis-route.
    pub fn apply_video(
        &mut self,
        drive: &mut CompositorDrive<Nv12Image>,
        batch: &[RouteIntent],
    ) -> Result<usize> {
        // Coalesce: keep only the LAST video route per cell.
        let mut last_per_cell: HashMap<&str, &StreamRef> = HashMap::new();
        let mut order: Vec<&str> = Vec::new();
        for intent in batch {
            if let RouteIntent::Video { cell, source } = intent {
                if last_per_cell.insert(cell.as_str(), source).is_none() {
                    order.push(cell.as_str());
                }
            }
        }
        let mut applied = 0usize;
        for cell in order {
            let Some(source) = last_per_cell.get(cell) else {
                continue;
            };
            let store_key = self.resolve_video_key(source)?;
            drive.rebind_cell(cell, &store_key)?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Resolve a video `StreamRef` to its `CompositorDrive` store key, validating
    /// it against the inventory when known.
    fn resolve_video_key(&self, source: &StreamRef) -> Result<String> {
        // When the inventory is known, the selector must resolve to a concrete
        // stream of the kind — otherwise it is an honest route error, not a
        // silent no-op.
        if self.resolution.inventory(&source.input_id).is_some()
            && self.resolution.validate_ref(source).is_none()
        {
            return Err(Error::Route(format!(
                "video route source {source:?} did not resolve in the input inventory"
            )));
        }
        self.resolution
            .video_store_keys
            .get(&ref_key(source))
            .cloned()
            .ok_or_else(|| {
                Error::Route(format!(
                    "video route source {source:?} has no resolved store key"
                ))
            })
    }

    /// Apply the **audio** routes in `batch` onto `bus`, coalesced to one
    /// effective re-point per target (the last route per target wins). Re-points
    /// the bus channel's `Arc<AudioStore>` with an equal-power cross-fade over
    /// `ramp_frames` (RT-9 pop-avoidance; `ramp_frames == 0` is a hard cut).
    /// Returns the number of targets actually re-pointed.
    ///
    /// # Errors
    ///
    /// [`Error::Route`] when a route's `StreamRef` does not resolve, the target
    /// channel is unknown, or the bus re-point fails.
    pub fn apply_audio(
        &mut self,
        bus: &mut ProgramBus,
        batch: &[RouteIntent],
        ramp_frames: usize,
    ) -> Result<usize> {
        let mut last_per_target: HashMap<&str, &StreamRef> = HashMap::new();
        let mut order: Vec<&str> = Vec::new();
        for intent in batch {
            if let RouteIntent::Audio { target, source, .. } = intent {
                if last_per_target.insert(target.as_str(), source).is_none() {
                    order.push(target.as_str());
                }
            }
        }
        let mut applied = 0usize;
        for target in order {
            let Some(source) = last_per_target.get(target) else {
                continue;
            };
            self.validate_audio_ref(source)?;
            let store = self
                .resolution
                .audio_stores
                .get(&ref_key(source))
                .ok_or_else(|| {
                    Error::Route(format!(
                        "audio route source {source:?} has no resolved store"
                    ))
                })?;
            let point = *self.resolution.audio_channels.get(target).ok_or_else(|| {
                Error::Route(format!(
                    "audio route target {target:?} is not a bus channel"
                ))
            })?;
            let tier = bus
                .repoint_crossfade(point, Arc::clone(store), ramp_frames)
                .map_err(|e| Error::Route(format!("audio bus re-point failed: {e}")))?;
            debug_assert!(matches!(tier, SwitchTier::ClickFree | SwitchTier::SoftStep));
            applied += 1;
        }
        Ok(applied)
    }

    fn validate_audio_ref(&self, source: &StreamRef) -> Result<()> {
        if self.resolution.inventory(&source.input_id).is_some()
            && self.resolution.validate_ref(source).is_none()
        {
            return Err(Error::Route(format!(
                "audio route source {source:?} did not resolve in the input inventory"
            )));
        }
        Ok(())
    }

    /// Apply the **subtitle** routes in `batch` onto the addressed layers,
    /// coalesced to one effective re-point per layer (the last route per layer
    /// wins). Re-points the layer's `Arc<dyn CueSource>` with CLEAR-on-switch
    /// (RT-10a). Returns the number of layers actually re-pointed.
    ///
    /// # Errors
    ///
    /// [`Error::Route`] when a route's `StreamRef` does not resolve, or the layer
    /// id is unknown.
    pub fn apply_subtitle(
        &mut self,
        layers: &HashMap<String, &SubtitleLayer>,
        batch: &[RouteIntent],
    ) -> Result<usize> {
        let mut last_per_layer: HashMap<&str, &StreamRef> = HashMap::new();
        let mut order: Vec<&str> = Vec::new();
        for intent in batch {
            if let RouteIntent::Subtitle { layer, source } = intent {
                if last_per_layer.insert(layer.as_str(), source).is_none() {
                    order.push(layer.as_str());
                }
            }
        }
        let mut applied = 0usize;
        for layer_id in order {
            let Some(source) = last_per_layer.get(layer_id) else {
                continue;
            };
            if self.resolution.inventory(&source.input_id).is_some()
                && self.resolution.validate_ref(source).is_none()
            {
                return Err(Error::Route(format!(
                    "subtitle route source {source:?} did not resolve in the input inventory"
                )));
            }
            let cue = self
                .resolution
                .cue_sources
                .get(&ref_key(source))
                .ok_or_else(|| {
                    Error::Route(format!(
                        "subtitle route source {source:?} has no resolved cue source"
                    ))
                })?;
            let layer = layers.get(layer_id).ok_or_else(|| {
                Error::Route(format!("subtitle route layer {layer_id:?} is unknown"))
            })?;
            layer.repoint(Arc::clone(cue));
            applied += 1;
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use multiview_core::stream::{StableStreamId, StreamDetail};

    fn audio_desc(lang: Option<&str>, channels: u16, pid: u16) -> StreamDescriptor {
        let mut d = StreamDescriptor::new(
            StableStreamId::from_ts_pid(StreamKind::Audio, pid),
            StreamKind::Audio,
            "aac",
            StreamDetail::Audio {
                channels,
                sample_rate: 48_000,
            },
        );
        if let Some(l) = lang {
            d = d.with_language(multiview_core::stream::Bcp47::parse(l).ok());
        }
        d
    }

    #[test]
    fn language_matches_iso_639_pairs() {
        assert!(language_matches("en", "eng"));
        assert!(language_matches("eng", "en"));
        assert!(language_matches("fr-CA", "fra"));
        assert!(!language_matches("en", "fr"));
    }

    #[test]
    fn best_resolves_to_the_default_flagged_stream() {
        let inv = StreamInventory::from_streams(vec![
            audio_desc(Some("fra"), 2, 300),
            audio_desc(Some("eng"), 6, 301).with_default(true),
        ]);
        let best = resolve_selector(&inv, StreamKind::Audio, &StreamSelector::Best).unwrap();
        assert_eq!(best.detail.audio_layout(), Some((6, 48_000)));
    }
}
