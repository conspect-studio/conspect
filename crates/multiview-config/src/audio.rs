//! Audio routing configuration (config-as-code).
//!
//! This module owns the **declarative** audio-routing schema (ADR-R005 §4.1):
//! a program-bus mix (which inputs contribute, at what gain, muted or not),
//! clean discrete per-input tracks (each routed to a named output track), and
//! the per-output audio selection (which tracks/mode each sink carries). It is
//! the config-as-code half — the mix/encode/mux **runtime** that consumes these
//! routes lives in `multiview-audio`'s `Mixer` and the engine (AUD-3/AUD-4) and
//! is not modelled here. The per-output *capability matrix* (TS = N tracks, HLS
//! = select-one, RTMP = endpoint-gated, NDI = channel-map) lives in
//! `multiview-audio`'s `capability` module, referencing the audio channel-layout
//! types; this crate validates only the document's internal reference
//! consistency.
//!
//! All unions are **internally tagged** by `kind`/`mode` (`#[serde(tag = …)]`),
//! never `untagged` (ADR-0010): the only encoding robust across the
//! self-describing JSON wire form and non-self-describing TOML.
//!
//! Levels are carried as **dB** (`f32`) — a gain is a level, not a rate — while
//! the working **sample rate** is an exact integer in Hz (never a float fps-style
//! rational; a rate is a whole number of samples per second, invariant #3's
//! "never float" applied to the audio clock).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The channel layout requested for one routed input, internally tagged by
/// `kind`. Mirrors the **variants** of `multiview_audio::ChannelLayout` (kept
/// independent here so the pure config schema does not depend on the audio
/// crate), but uses `snake_case` tags per the config-as-code convention whereas
/// the audio crate's own `ChannelLayout` serializes its tags `PascalCase`; the
/// runtime maps between them by variant, never by a serde round-trip.
///
/// Only the layouts the program bus and discrete-track model need are
/// enumerated; the audio crate's capability matrix maps these onto a concrete
/// transport (e.g. >2-channel AAC-over-NDI is capped — validated there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioChannels {
    /// Single channel.
    Mono,
    /// Two channels: L, R.
    Stereo,
    /// Six channels: L, R, C, LFE, Ls, Rs (the BS.1770 5.1 ordering).
    FivePointOne,
}

impl AudioChannels {
    /// Number of channels in this layout.
    #[must_use]
    pub const fn channel_count(self) -> u32 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::FivePointOne => 6,
        }
    }
}

/// One per-input audio route (ADR-R005 §4.1).
///
/// An input fans out to (a) a clean **discrete track** (named by
/// [`target_track`](AudioRoute::target_track)) carried where the output supports
/// it, and (b) the mixed **program bus** when
/// [`include_in_program_bus`](AudioRoute::include_in_program_bus) is set, scaled
/// by [`gain_db`](AudioRoute::gain_db). A [`mute`](AudioRoute::mute)d route
/// contributes silence to the bus while keeping its discrete track wired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioRoute {
    /// The managed source id (`sources[].id`) this route takes audio from.
    pub input_id: String,
    /// The channel layout requested for this input.
    pub channels: AudioChannels,
    /// The named discrete output track this input is routed to (if any). `None`
    /// ⇒ this input contributes to the program bus only (no clean track).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_track: Option<String>,
    /// ISO-639 language tag advertised for the discrete track (e.g. `"eng"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Human-friendly track title (e.g. `"Camera A"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether this input contributes to the mixed program bus.
    #[serde(default)]
    pub include_in_program_bus: bool,
    /// Program-bus contribution gain in dB (a level, not a rate; `0.0` ⇒ unity).
    #[serde(default)]
    pub gain_db: f32,
    /// Whether this input is muted (contributes silence to the program bus; its
    /// discrete track, if any, is still declared).
    #[serde(default)]
    pub mute: bool,
}

impl AudioRoute {
    /// Validate this route's own fields in isolation: a non-empty `input_id`, a
    /// finite `gain_db`, and a non-empty `target_track` when one is named.
    ///
    /// Reference resolution (does `input_id` name a declared source? does
    /// `target_track` collide with another route?) is the routing block's
    /// responsibility and is enforced by [`AudioRouting::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an empty `input_id`, a non-finite
    /// `gain_db`, or an empty `target_track`/`language`/`title`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.input_id.is_empty() {
            return Err(ConfigError::Validation(
                "an audio route has an empty input_id".to_owned(),
            ));
        }
        if !self.gain_db.is_finite() {
            return Err(ConfigError::Validation(format!(
                "audio route for {:?}: gain_db must be finite (got {})",
                self.input_id, self.gain_db
            )));
        }
        if let Some(track) = &self.target_track {
            if track.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "audio route for {:?}: target_track is empty",
                    self.input_id
                )));
            }
        }
        if matches!(self.language.as_deref(), Some("")) {
            return Err(ConfigError::Validation(format!(
                "audio route for {:?}: language is empty",
                self.input_id
            )));
        }
        if matches!(self.title.as_deref(), Some("")) {
            return Err(ConfigError::Validation(format!(
                "audio route for {:?}: title is empty",
                self.input_id
            )));
        }
        Ok(())
    }

    /// Whether this route actually contributes signal to the program bus: it is
    /// included and not muted. A muted (or unincluded) route is silent on the
    /// bus regardless of its `gain_db`.
    #[must_use]
    pub const fn contributes_to_program(&self) -> bool {
        self.include_in_program_bus && !self.mute
    }
}

/// The whole-document audio routing block (ADR-R005 §4.1).
///
/// Carries the working sample rate (exact integer Hz), and the set of per-input
/// routes that define the program bus membership and the discrete tracks. The
/// per-output *selection* of these tracks lives on each [`crate::Output`] as an
/// [`OutputAudio`] block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioRouting {
    /// The working/program-bus sample rate in Hz (exact integer; ADR-R005's
    /// canonical resample target is 48 000). Never a float.
    pub sample_rate_hz: u32,
    /// The per-input routes.
    #[serde(default)]
    pub routes: Vec<AudioRoute>,
}

impl AudioRouting {
    /// The set of discrete track names this routing declares (the program bus,
    /// `"prog"`, is always available as a selectable track in addition to these).
    #[must_use]
    pub fn declared_tracks(&self) -> Vec<&str> {
        let mut tracks: Vec<&str> = vec![PROGRAM_TRACK];
        for route in &self.routes {
            if let Some(track) = &route.target_track {
                if !tracks.contains(&track.as_str()) {
                    tracks.push(track.as_str());
                }
            }
        }
        tracks
    }

    /// Validate the routing block against the declared `source_ids` and the set
    /// of `selectable_tracks` (the union of program bus + every named track) the
    /// caller wishes to expose.
    ///
    /// Enforces:
    /// - the working sample rate is non-zero;
    /// - each route is internally consistent ([`AudioRoute::validate`]);
    /// - every route's `input_id` resolves to a declared source;
    /// - no two routes share the same `input_id` (a duplicate declaration);
    /// - no two routes claim the same `target_track` (ambiguous discrete wiring);
    /// - the program bus has at least one contributing (included, non-muted)
    ///   member when any route asks to be on the bus — an all-muted bus is a
    ///   silent program an operator almost never intends.
    ///
    /// `selectable_tracks` is accepted so the caller can pre-compute the
    /// document-wide selectable set; the routing's own
    /// [`declared_tracks`](AudioRouting::declared_tracks) is a subset of it.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the first violated invariant.
    pub fn validate(
        &self,
        source_ids: &[&str],
        selectable_tracks: &[&str],
    ) -> Result<(), ConfigError> {
        if self.sample_rate_hz == 0 {
            return Err(ConfigError::Validation(
                "audio.sample_rate_hz must be > 0".to_owned(),
            ));
        }

        let sources: HashSet<&str> = source_ids.iter().copied().collect();
        let selectable: HashSet<&str> = selectable_tracks.iter().copied().collect();

        let mut seen_inputs: HashSet<&str> = HashSet::with_capacity(self.routes.len());
        let mut seen_tracks: HashSet<&str> = HashSet::with_capacity(self.routes.len());
        let mut any_program_request = false;
        let mut any_program_contributor = false;

        for route in &self.routes {
            route.validate()?;

            if !sources.contains(route.input_id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "audio route binds unknown source input_id {:?}",
                    route.input_id
                )));
            }
            if !seen_inputs.insert(route.input_id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate audio route for input_id {:?}",
                    route.input_id
                )));
            }
            if let Some(track) = &route.target_track {
                if track == PROGRAM_TRACK {
                    return Err(ConfigError::Validation(format!(
                        "audio route for {:?}: target_track {PROGRAM_TRACK:?} is reserved for the \
                         mixed program bus",
                        route.input_id
                    )));
                }
                if !seen_tracks.insert(track.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "two audio routes target the same discrete track {track:?}"
                    )));
                }
                // A declared track must be in the selectable set the document
                // exposes (it always is, since `selectable_tracks` is derived to
                // include it; this guards a caller passing a stale set).
                if !selectable.contains(track.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "audio route for {:?}: target_track {track:?} is not a selectable track",
                        route.input_id
                    )));
                }
            }

            if route.include_in_program_bus {
                any_program_request = true;
                if route.contributes_to_program() {
                    any_program_contributor = true;
                }
            }
        }

        if any_program_request && !any_program_contributor {
            return Err(ConfigError::Validation(
                "the program bus has no contributing member (every included input is muted)"
                    .to_owned(),
            ));
        }

        Ok(())
    }
}

/// The reserved name of the mixed **program bus** track, always selectable in
/// addition to the named discrete tracks.
pub const PROGRAM_TRACK: &str = "prog";

/// How an output selects audio, internally tagged by `mode`.
///
/// `program` carries only the mixed program bus; `tracks` carries an explicit
/// list of selectable tracks (program + named discrete). Which of these an
/// output transport can actually deliver is decided by the per-output capability
/// matrix in `multiview-audio` (e.g. an HLS output is select-one; an RTMP output
/// is endpoint-gated) — this schema only declares the operator's intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputAudioMode {
    /// Carry the mixed program bus **plus every declared discrete track**
    /// (`[audio]` routes with a `target_track`). The `tracks` list is ignored.
    Auto,
    /// Carry only the mixed program bus.
    Program,
    /// Carry an explicit list of selectable tracks.
    Tracks,
}

/// The per-output audio selection block (attached to a [`crate::Output`]).
///
/// In [`OutputAudioMode::Program`] the `tracks` list is ignored (the output
/// carries the program bus). In [`OutputAudioMode::Tracks`] every name in
/// `tracks` must resolve to a selectable track declared by the routing block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputAudio {
    /// How this output selects audio.
    pub mode: OutputAudioMode,
    /// The explicit selectable-track list (used only in
    /// [`OutputAudioMode::Tracks`]).
    #[serde(default)]
    pub tracks: Vec<String>,
}

impl OutputAudio {
    /// Validate this output's audio selection against the document's
    /// `selectable_tracks` (program bus + every named discrete track).
    ///
    /// Enforces that, in [`OutputAudioMode::Tracks`], the list is non-empty,
    /// carries no duplicates, and every name resolves to a selectable track.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the offending output/track.
    pub fn validate(
        &self,
        output_label: &str,
        selectable_tracks: &[&str],
    ) -> Result<(), ConfigError> {
        if self.mode != OutputAudioMode::Tracks {
            return Ok(());
        }
        if self.tracks.is_empty() {
            return Err(ConfigError::Validation(format!(
                "output {output_label:?}: audio mode is `tracks` but no tracks are selected"
            )));
        }
        let selectable: HashSet<&str> = selectable_tracks.iter().copied().collect();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.tracks.len());
        for track in &self.tracks {
            if !seen.insert(track.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "output {output_label:?}: selects track {track:?} more than once"
                )));
            }
            if !selectable.contains(track.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "output {output_label:?}: selects unknown audio track {track:?}"
                )));
            }
        }
        Ok(())
    }

    /// The number of **discrete** tracks this selection carries: the selected
    /// tracks that are not the mixed program bus ([`PROGRAM_TRACK`]).
    ///
    /// In [`OutputAudioMode::Program`] this is always `0` (the output carries
    /// only the mixed bus). This is the count the per-output capability matrix
    /// ([`OutputAudioCapability`]) gates: a transport that cannot carry
    /// simultaneous discrete tracks (NDI) or only carries one (legacy RTMP)
    /// rejects a selection whose discrete count exceeds its capacity.
    #[must_use]
    pub fn discrete_track_count(&self) -> u32 {
        if self.mode != OutputAudioMode::Tracks {
            // `auto` adapts to what the transport carries (resolved at run
            // build), so it never fails the capability gate; `program` is 0.
            return 0;
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.tracks.len());
        let mut count: u32 = 0;
        for track in &self.tracks {
            if track == PROGRAM_TRACK {
                continue;
            }
            // De-duplicate so a (malformed) repeated name is not double-counted;
            // the reference validator above rejects duplicates independently.
            if seen.insert(track.as_str()) {
                count = count.saturating_add(1);
            }
        }
        count
    }

    /// Resolve this selection against the document's `declared` discrete track
    /// names (in route order, excluding [`PROGRAM_TRACK`]): whether the output
    /// carries the program bus, and which discrete tracks (in declared order).
    #[must_use]
    pub fn resolve<'a>(&self, declared: &[&'a str]) -> (bool, Vec<&'a str>) {
        match self.mode {
            OutputAudioMode::Program => (true, Vec::new()),
            OutputAudioMode::Auto => (true, declared.to_vec()),
            OutputAudioMode::Tracks => (
                self.tracks.iter().any(|t| t == PROGRAM_TRACK),
                declared
                    .iter()
                    .copied()
                    .filter(|d| self.tracks.iter().any(|t| t == d))
                    .collect(),
            ),
        }
    }

    /// Validate this output's audio selection against the transport's
    /// **capability** (ADR-R005 §4.2): the selectable-track *references* must be
    /// consistent (delegated to [`OutputAudio::validate`]) **and** the count of
    /// requested discrete tracks must fit what the transport can actually
    /// deliver.
    ///
    /// This is the capability cross-check the brief's "designed-in asymmetry"
    /// requires: N simultaneous discrete tracks pass on MPEG-TS/SRT and RTSP,
    /// are rejected on NDI (channel-map only) and a legacy RTMP endpoint, and are
    /// accepted on an Enhanced-RTMP endpoint that declares multitrack. HLS is
    /// select-one — multiple tracks are a *selector*, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an inconsistent reference (see
    /// [`OutputAudio::validate`]), or [`ConfigError::AudioCapability`] when the
    /// discrete-track count exceeds the transport's
    /// [`discrete_capacity`](OutputAudioCapability::discrete_capacity).
    pub fn validate_against_capability(
        &self,
        output_label: &str,
        selectable_tracks: &[&str],
        capability: OutputAudioCapability,
    ) -> Result<(), ConfigError> {
        self.validate(output_label, selectable_tracks)?;

        let requested = self.discrete_track_count();
        if !capability.discrete_capacity.accepts(requested) {
            return Err(ConfigError::AudioCapability {
                output: output_label.to_owned(),
                reason: capability.refusal_reason(requested),
            });
        }
        Ok(())
    }
}

/// How a transport delivers discrete audio tracks (ADR-R005 §4.2). This is the
/// machine-readable delivery semantics the validator and the Web UI (AUD-8) both
/// read, so an impossible selection is greyed out without re-deriving the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TrackDelivery {
    /// All selected discrete tracks are delivered **at once** (MPEG-TS/SRT PIDs,
    /// RTSP `m=audio` subsessions): the UI shows simultaneous monitors.
    Simultaneous,
    /// N tracks are carried but the player plays **one at a time** (HLS/DASH
    /// renditions): the UI is a track *selector*, not simultaneous monitoring.
    SelectOne,
    /// No selectable discrete tracks at all — channels only (NDI channel-map).
    /// A discrete-track selection on such a transport is a capability error.
    None,
}

/// How many discrete audio tracks a transport can carry (ADR-R005 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TrackCapacity {
    /// No practical cap (MPEG-TS/SRT, RTSP, HLS renditions).
    Unlimited,
    /// At most `n` discrete tracks (legacy RTMP = `1`; NDI channel-map = `0`).
    AtMost(u32),
}

impl TrackCapacity {
    /// Whether this capacity admits `requested` discrete tracks.
    #[must_use]
    pub const fn accepts(self, requested: u32) -> bool {
        match self {
            Self::Unlimited => true,
            Self::AtMost(n) => requested <= n,
        }
    }
}

/// One output's audio selection as rewritten by [`propagate_routing_change`]:
/// which discrete tracks were dropped (a route removed its `target_track`, or
/// the whole route disappeared) and which were renamed (the same `input_id`
/// kept a route but `target_track` changed name).
///
/// `output_index` is the output's position in the `outputs` slice
/// [`propagate_routing_change`] was called with — the caller (the control
/// plane, which alone knows the stable REST output id for that slot) maps it
/// onto whatever identity its report should name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct OutputAudioUpdate {
    /// This output's position in the `outputs` slice passed to
    /// [`propagate_routing_change`].
    pub output_index: usize,
    /// Track names this output's selection no longer carries (dropped from
    /// `tracks`, with no equivalent rename).
    pub removed: Vec<String>,
    /// `(old_name, new_name)` pairs this output's selection was rewritten to
    /// follow.
    pub renamed: Vec<(String, String)>,
}

/// Compute the discrete-track renames and removals a routing-document replace
/// implies, relative to the document it replaces.
///
/// A **rename** is a route whose `input_id` is unchanged between `old` and
/// `new` but whose `target_track` differs (both present). A **removal** is a
/// discrete track name that existed in `old`'s declared tracks but has no
/// surviving route in `new` with that exact name as its `target_track` (the
/// route lost its track, changed `input_id`, or the route itself was
/// removed) **and** is not the target of a rename (a renamed source is not
/// also counted as a bare removal of its old name).
///
/// `old` is `None` for a first-ever routing document (nothing to rename or
/// remove relative to).
#[must_use]
pub fn diff_track_changes(
    old: Option<&AudioRouting>,
    new: &AudioRouting,
) -> (Vec<(String, String)>, Vec<String>) {
    let Some(old) = old else {
        return (Vec::new(), Vec::new());
    };

    let mut renamed: Vec<(String, String)> = Vec::new();
    for old_route in &old.routes {
        let Some(old_track) = &old_route.target_track else {
            continue;
        };
        if let Some(new_route) = new
            .routes
            .iter()
            .find(|route| route.input_id == old_route.input_id)
        {
            if let Some(new_track) = &new_route.target_track {
                if new_track != old_track {
                    renamed.push((old_track.clone(), new_track.clone()));
                }
            }
        }
    }

    let new_targets: HashSet<&str> = new
        .routes
        .iter()
        .filter_map(|route| route.target_track.as_deref())
        .collect();
    let renamed_old: HashSet<&str> = renamed.iter().map(|(old, _)| old.as_str()).collect();
    let removed: Vec<String> = old
        .routes
        .iter()
        .filter_map(|route| route.target_track.as_deref())
        .filter(|track| !new_targets.contains(track) && !renamed_old.contains(track))
        .map(ToOwned::to_owned)
        .collect();

    (renamed, removed)
}

/// Apply a set of renames/removals to one output's explicit `tracks` list
/// (mode `tracks` only — `auto`/`program` carry no explicit list to rewrite).
/// Returns the rewritten list and whether anything actually changed.
///
/// A track renamed to a name the list already carries collapses (no
/// duplicate); an emptied `tracks` list falls back to `[PROGRAM_TRACK]`
/// (ADR-R005 §4.3: an output must always resolve to *something* audible)
/// rather than landing an invalid `mode = "tracks"` selection with no tracks.
fn rewrite_tracks(
    tracks: &[String],
    renamed: &[(String, String)],
    removed: &[String],
) -> (Vec<String>, bool) {
    let mut changed = false;
    let mut next: Vec<String> = Vec::with_capacity(tracks.len());
    for track in tracks {
        if removed.iter().any(|r| r == track) {
            changed = true;
            continue;
        }
        let mapped = renamed
            .iter()
            .find(|(old, _)| old == track)
            .map_or_else(|| track.clone(), |(_, new)| new.clone());
        if &mapped != track {
            changed = true;
        }
        if next.contains(&mapped) {
            changed = true;
        } else {
            next.push(mapped);
        }
    }
    if next.is_empty() && !tracks.is_empty() {
        changed = true;
        next.push(PROGRAM_TRACK.to_owned());
    }
    (next, changed)
}

/// Rewrite every output's `mode = "tracks"` selection to follow a routing
/// replace's renames/removals, **in place**, returning one
/// [`OutputAudioUpdate`] per output actually changed (ADR-R005 §4.3: the
/// preferred fix for the rename/remove deadlock — the atomic commit updates
/// BOTH the routing document and every referencing output's selection
/// together, instead of requiring the operator to stage an intermediate
/// state by hand).
///
/// `outputs` is addressed by its index; the caller maps `output_index` back
/// onto the stable REST/API identity for that slot.
pub fn propagate_routing_change(
    old: Option<&AudioRouting>,
    new: &AudioRouting,
    outputs: &mut [crate::Output],
) -> Vec<OutputAudioUpdate> {
    let (renamed, removed) = diff_track_changes(old, new);
    if renamed.is_empty() && removed.is_empty() {
        return Vec::new();
    }

    let mut updates = Vec::new();
    for (output_index, output) in outputs.iter_mut().enumerate() {
        let Some(selection) = output.audio_mut().as_mut() else {
            continue;
        };
        if selection.mode != OutputAudioMode::Tracks {
            continue;
        }
        let (next_tracks, changed) = rewrite_tracks(&selection.tracks, &renamed, &removed);
        if !changed {
            continue;
        }
        let this_removed: Vec<String> = removed
            .iter()
            .filter(|track| selection.tracks.contains(track))
            .cloned()
            .collect();
        let this_renamed: Vec<(String, String)> = renamed
            .iter()
            .filter(|(old_track, _)| selection.tracks.contains(old_track))
            .cloned()
            .collect();
        selection.tracks = next_tracks;
        updates.push(OutputAudioUpdate {
            output_index,
            removed: this_removed,
            renamed: this_renamed,
        });
    }
    updates
}

/// One `[[routing.output]]` audio feed rewritten by
/// [`propagate_routing_feed_change`] (ADR-0108 §5.5: the ADR-R005 §4.3 rename
/// propagation extends to output audio feeds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct OutputFeedAudioUpdate {
    /// The routed output's id (`[[routing.output]] output`).
    pub output: String,
    /// Track names the feed no longer carries.
    pub removed: Vec<String>,
    /// `(old_name, new_name)` pairs the feed was rewritten to follow.
    pub renamed: Vec<(String, String)>,
    /// The feed emptied and became `program`.
    pub became_program: bool,
}

/// Rewrite every `[[routing.output]] audio = { tracks = [..] }` feed to follow
/// a routing replace's renames and removals, **in place** (the sibling of
/// [`propagate_routing_change`] for output feeds). A track renamed to a name
/// the feed already carries collapses; a feed whose every track was removed
/// becomes `program` (the output stays audible). Returns one update per feed
/// actually changed.
#[must_use]
pub fn propagate_routing_feed_change(
    old: Option<&AudioRouting>,
    new: &AudioRouting,
    routing: Option<&mut crate::RoutingTable>,
) -> Vec<OutputFeedAudioUpdate> {
    let (renamed, removed) = diff_track_changes(old, new);
    let Some(routing) = routing else {
        return Vec::new();
    };
    if renamed.is_empty() && removed.is_empty() {
        return Vec::new();
    }
    let mut updates = Vec::new();
    for xp in &mut routing.output {
        let crate::AudioFeed::Tracks(tracks) = &xp.audio else {
            continue;
        };
        let mut next: Vec<String> = Vec::with_capacity(tracks.len());
        for track in tracks {
            if removed.contains(track) {
                continue;
            }
            let mapped = renamed
                .iter()
                .find(|(old, _)| old == track)
                .map_or_else(|| track.clone(), |(_, new)| new.clone());
            if !next.contains(&mapped) {
                next.push(mapped);
            }
        }
        if next == *tracks {
            continue;
        }
        let update = OutputFeedAudioUpdate {
            output: xp.output.clone(),
            removed: removed
                .iter()
                .filter(|t| tracks.contains(t))
                .cloned()
                .collect(),
            renamed: renamed
                .iter()
                .filter(|(old, _)| tracks.contains(old))
                .cloned()
                .collect(),
            became_program: next.is_empty(),
        };
        xp.audio = if next.is_empty() {
            crate::AudioFeed::Program
        } else {
            crate::AudioFeed::Tracks(next)
        };
        updates.push(update);
    }
    updates
}

/// The **per-output audio capability** — the verified ADR-R005 §4.2 matrix as a
/// first-class, machine-readable value (the brief: "a first-class data
/// structure, not scattered conditionals").
///
/// Derived from an [`crate::Output`] by [`crate::Output::audio_capability`] and
/// consumed both by config-time validation
/// ([`OutputAudio::validate_against_capability`]) and by the Web UI routing matrix
/// (AUD-8), which greys out cells a transport cannot deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OutputAudioCapability {
    /// How discrete tracks are delivered (simultaneous, select-one, or none).
    pub delivery: TrackDelivery,
    /// How many discrete tracks the transport can carry.
    pub discrete_capacity: TrackCapacity,
    /// The maximum number of audio **channels** the transport can carry in one
    /// stream, if bounded (e.g. AAC-over-NDI is capped at 2). `None` ⇒ no
    /// transport-imposed channel cap modelled here. Reserved for the per-channel
    /// downmix check the Web UI surfaces; not yet gated by this validator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_channels: Option<u32>,
}

impl OutputAudioCapability {
    /// Construct a capability with no channel cap.
    #[must_use]
    pub const fn new(delivery: TrackDelivery, discrete_capacity: TrackCapacity) -> Self {
        Self {
            delivery,
            discrete_capacity,
            max_channels: None,
        }
    }

    /// A human-readable explanation of why `requested` discrete tracks exceed
    /// this capability — used as the [`ConfigError::AudioCapability`] reason.
    #[must_use]
    pub fn refusal_reason(self, requested: u32) -> String {
        match (self.delivery, self.discrete_capacity) {
            (TrackDelivery::None, _) => format!(
                "this transport carries no selectable discrete tracks (channels only, e.g. an NDI \
                 channel-map); {requested} discrete track(s) were selected — carry the mixed \
                 program bus instead"
            ),
            (_, TrackCapacity::AtMost(n)) => format!(
                "this transport carries at most {n} discrete audio track(s) (e.g. a legacy RTMP \
                 endpoint); {requested} were selected — declare an endpoint that supports \
                 multitrack or degrade to the mixed program bus"
            ),
            (_, TrackCapacity::Unlimited) => {
                format!("this transport's audio capacity rejected {requested} discrete track(s)")
            }
        }
    }
}
