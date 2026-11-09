//! One admitted shared ladder published as real MoQ tracks on one broadcast.
//!
//! Every video rendition is its own track, all bound to the ladder's single configuration
//! generation and stamped against the ladder's single catalog clock. Nothing here encodes:
//! the caller hands in windows that came from the output owner's existing encoders, and a
//! window is published to one rendition. Switching a consumer between renditions is served
//! from the new track's next group, which the [`VideoAdapter`] only starts on a verified RAP.

use std::{collections::BTreeMap, fmt, time::Duration};

use moq_net::{Timescale, Timestamp};

use crate::{
    Packet, Reject, RetainedWindow, VideoAdapter, Window,
    ladder::{LadderError, MAX_AUDIO_RENDITIONS, MAX_VIDEO_RENDITIONS, SharedLadder},
};

/// One admitted tick of a shared ladder: a window per rung plus the independent audio.
///
/// The rung index is the ladder level, so a window can never describe a rung the ladder did
/// not admit. Audio travels once per tick, outside the rung index.
#[derive(Clone, Debug)]
pub struct LadderWindow {
    pub generation: u64,
    /// Rungs with media loss before this window; close their previous groups.
    pub discontinuities: u64,
    /// One window per admitted rung, in level order. A rung with no media this tick is empty.
    pub renditions: Vec<Window>,
    pub audio: BTreeMap<String, Vec<Packet>>,
}

impl LadderWindow {
    pub fn new(generation: u64, renditions: Vec<Window>) -> Self {
        Self {
            generation,
            discontinuities: 0,
            renditions,
            audio: BTreeMap::new(),
        }
    }

    /// Retained media bytes across every rung and every audio track.
    pub fn retained_bytes(&self) -> usize {
        self.renditions
            .iter()
            .flat_map(|window| window.packets.iter())
            .chain(self.audio.values().flatten())
            .map(|packet| packet.payload.len())
            .sum()
    }

    /// Retained packet count across every rung and every audio track.
    pub fn retained_packets(&self) -> usize {
        self.renditions
            .iter()
            .map(|window| window.packets.len())
            .sum::<usize>()
            + self.audio.values().map(Vec::len).sum::<usize>()
    }
}

impl RetainedWindow for LadderWindow {
    fn generation(&self) -> u64 {
        self.generation
    }

    /// Rung order, then audio in track-name order: deterministic and bounded by the ladder
    /// bounds, so the fence charges every track exactly once.
    fn for_each_packet(&mut self, visit: &mut dyn FnMut(&mut Packet)) {
        for window in &mut self.renditions {
            for packet in &mut window.packets {
                visit(packet);
            }
        }
        for packets in self.audio.values_mut() {
            for packet in packets {
                visit(packet);
            }
        }
    }
}

/// The ladder descriptor track, published beside `catalog.json`.
pub const LADDER_TRACK: &str = "ladder.json";

/// Upper bound on tracks this publication can render: every admitted rendition plus
/// independent audio, the media catalog and the ladder descriptor.
pub const MAX_PUBLISHED_TRACKS: usize = MAX_VIDEO_RENDITIONS + MAX_AUDIO_RENDITIONS + 2;

#[derive(Debug)]
pub enum PublishError {
    Ladder(LadderError),
    Moq(moq_net::Error),
    Reject(Reject),
    UnknownRendition(usize),
    UnknownAudioTrack(String),
    /// A microsecond timestamp could not be represented.
    Timestamp,
}

/// The catalog and ladder descriptor are written at PTS zero of the ladder's clock.
fn pts_zero() -> Result<Timestamp, PublishError> {
    Timestamp::new(0, Timescale::MICRO).map_err(|_| PublishError::Timestamp)
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ladder(error) => write!(f, "{error}"),
            Self::Moq(error) => write!(f, "moq error: {error}"),
            Self::Reject(reject) => write!(f, "rendition rejected the window: {reject:?}"),
            Self::UnknownRendition(level) => write!(f, "no admitted rendition at level {level}"),
            Self::UnknownAudioTrack(name) => write!(f, "no admitted audio track {name:?}"),
            Self::Timestamp => write!(f, "microsecond timestamp is not representable"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<LadderError> for PublishError {
    fn from(error: LadderError) -> Self {
        Self::Ladder(error)
    }
}
impl From<moq_net::Error> for PublishError {
    fn from(error: moq_net::Error) -> Self {
        Self::Moq(error)
    }
}
impl From<Reject> for PublishError {
    fn from(error: Reject) -> Self {
        Self::Reject(error)
    }
}
impl From<hang::Error> for PublishError {
    fn from(error: hang::Error) -> Self {
        Self::Ladder(LadderError::from(error))
    }
}

/// A live shared-ladder publication: bounded tracks, one clock, one generation.
pub struct SharedPublication {
    ladder: SharedLadder,
    allocation: Option<crate::allocation::AllocationBudget>,
    broadcast: moq_net::broadcast::Producer,
    video: Vec<moq_net::track::Producer>,
    adapters: Vec<VideoAdapter>,
    audio: BTreeMap<String, moq_net::track::Producer>,
    catalog: moq_net::track::Producer,
    ladder_track: moq_net::track::Producer,
    audio_last_pts: BTreeMap<String, u64>,
    stalled: Vec<usize>,
    windows: Vec<u64>,
}

impl SharedPublication {
    /// Render an admitted ladder into real tracks on an existing broadcast.
    pub fn create_with_budget(ladder: SharedLadder, broadcast: &moq_net::broadcast::Producer,
        budget: crate::allocation::AllocationBudget) -> Result<Self, PublishError> {
        let mut publication = Self::create(ladder, broadcast)?;
        for adapter in &mut publication.adapters { adapter.allocation = Some(budget.clone()); }
        publication.allocation = Some(budget);
        Ok(publication)
    }
    pub fn allocation_budget(&self) -> Option<&crate::allocation::AllocationBudget> { self.allocation.as_ref() }

    pub fn create(
        ladder: SharedLadder,
        broadcast: &moq_net::broadcast::Producer,
    ) -> Result<Self, PublishError> {
        let levels = ladder.len();
        let mut video = Vec::with_capacity(levels);
        let mut adapters = Vec::with_capacity(levels);
        for spec in ladder.renditions() {
            let track = broadcast.create_track(
                spec.name(),
                VideoAdapter::track_info().with_max_age(Duration::from_secs(2)),
            )?;
            adapters.push(VideoAdapter::new(track.clone(), ladder.generation()));
            video.push(track);
        }
        let mut audio = BTreeMap::new();
        for spec in ladder.audio() {
            audio.insert(
                spec.name().to_string(),
                broadcast.create_track(
                    spec.name(),
                    hang::container::track_info(hang::catalog::PRIORITY.audio),
                )?,
            );
        }
        let mut catalog = broadcast.create_track(
            hang::catalog::Catalog::<()>::DEFAULT_NAME,
            hang::catalog::Catalog::<()>::default_track_info(),
        )?;
        let mut ladder_track = broadcast.create_track(
            LADDER_TRACK,
            hang::container::track_info(hang::catalog::PRIORITY.video)
                .with_max_age(Duration::from_secs(2)),
        )?;
        let publication = Self {
            ladder,
            allocation: None,
            broadcast: broadcast.clone(),
            video,
            adapters,
            audio,
            catalog: catalog.clone(),
            ladder_track: ladder_track.clone(),
            audio_last_pts: BTreeMap::new(),
            stalled: Vec::new(),
            windows: vec![0; levels],
        };
        catalog.write_frame(pts_zero()?, publication.ladder.catalog_json()?)?;
        ladder_track.write_frame(pts_zero()?, publication.ladder.descriptor_json())?;
        Ok(publication)
    }

    pub fn ladder(&self) -> &SharedLadder {
        &self.ladder
    }
    pub fn generation(&self) -> u64 {
        self.ladder.generation()
    }
    pub fn broadcast(&self) -> &moq_net::broadcast::Producer {
        &self.broadcast
    }

    /// How many tracks this publication renders, catalogs included.
    pub fn published_track_count(&self) -> usize {
        self.video.len() + self.audio.len() + 2
    }

    /// Windows published per rendition. Independent of how many peers are subscribed:
    /// the publisher encodes once and serves that same window to every peer.
    pub fn published_windows(&self) -> &[u64] {
        &self.windows
    }

    pub fn rendition_track(&self, level: usize) -> Option<&moq_net::track::Producer> {
        self.video.get(level)
    }
    pub fn audio_track(&self, name: &str) -> Option<&moq_net::track::Producer> {
        self.audio.get(name)
    }
    pub fn catalog_track(&self) -> &moq_net::track::Producer {
        &self.catalog
    }
    pub fn ladder_track(&self) -> &moq_net::track::Producer {
        &self.ladder_track
    }
    pub fn stalled(&self) -> &[usize] {
        &self.stalled
    }

    /// Opt into bounded trial cancellation on an existing non-base rendition.
    /// This never creates tracks/encoders and never changes independent audio.
    pub fn enable_trial_cancellation(&mut self, level: usize) -> Result<(), PublishError> {
        if level == 0 { return Err(PublishError::UnknownRendition(level)); }
        self.adapters.get_mut(level).ok_or(PublishError::UnknownRendition(level))?
            .enable_trial_cancellation();
        Ok(())
    }

    /// Cancel only the selected video rung's current and bounded retained groups.
    /// The caller must separately enforce actual transport allocation limits.
    pub fn cancel_trial(&mut self, level: usize) -> Result<(), PublishError> {
        if level == 0 { return Err(PublishError::UnknownRendition(level)); }
        self.adapters.get_mut(level).ok_or(PublishError::UnknownRendition(level))?.abort_group();
        Ok(())
    }

    /// Publish one already-admitted window to one rendition. Never encodes and never copies
    /// the payload: the shared `Bytes` allocation stays shared through the transport.
    pub fn publish(&mut self, level: usize, window: &Window) -> Result<(), PublishError> {
        let adapter = self
            .adapters
            .get_mut(level)
            .ok_or(PublishError::UnknownRendition(level))?;
        adapter.publish(window)?;
        self.windows[level] += 1;
        Ok(())
    }

    /// Rung names in level order.
    pub fn rendition_names(&self) -> Vec<&str> {
        self.ladder
            .renditions()
            .iter()
            .map(crate::ladder::RenditionSpec::name)
            .collect()
    }

    /// Independent audio track names.
    pub fn audio_names(&self) -> Vec<&str> {
        self.audio.keys().map(String::as_str).collect()
    }

    /// Publish the independent audio for one tick. Audio is written once per tick regardless
    /// of which rungs are active, and a rendition switch never touches it.
    pub fn publish_audio(&mut self, name: &str, packets: &[Packet]) -> Result<(), PublishError> {
        let previous = self.audio_last_pts.get(name).copied();
        let mut last = previous;
        let track = self
            .audio
            .get(name)
            .ok_or(PublishError::UnknownAudioTrack(name.to_string()))?;
        let mut prepared = Vec::with_capacity(packets.len());
        for packet in packets {
            if last.is_some_and(|pts| packet.pts_us <= pts) {
                return Err(PublishError::Reject(Reject::Timestamp));
            }
            let timestamp = moq_net::Timestamp::new(packet.pts_us, Timescale::MICRO)
                .map_err(|_| PublishError::Timestamp)?;
            let serialized = crate::provider::serialized_frame_in(packet, self.allocation.as_ref())?;
            prepared.push((timestamp, serialized));
            last = Some(packet.pts_us);
        }
        for (timestamp, serialized) in prepared {
            let mut group = track.append_group()?;
            group.write_frame(timestamp, serialized)?;
            group.finish()?;
        }
        if let Some(last) = last {
            self.audio_last_pts.insert(name.to_string(), last);
        }
        Ok(())
    }

    /// Set the levels the publisher recommends avoiding. The tracks stay published, so a
    /// shortfall in uplink budget is reported explicitly instead of shrinking production.
    pub fn set_stalled(&mut self, levels: &[usize]) -> Result<(), PublishError> {
        for level in levels {
            if *level >= self.video.len() {
                return Err(PublishError::UnknownRendition(*level));
            }
        }
        let mut stalled = levels.to_vec();
        stalled.sort_unstable();
        stalled.dedup();
        self.catalog.write_frame(
            pts_zero()?,
            self.ladder.catalog_with_stalled(&stalled)?.to_vec()?,
        )?;
        self.stalled = stalled;
        Ok(())
    }

    /// Signal a lost or gapped window for one rendition: its next published window must
    /// start at a verified RAP. Other renditions are unaffected, and production continues.
    pub fn discontinuity(&mut self, level: usize) -> Result<(), PublishError> {
        let adapter = self
            .adapters
            .get_mut(level)
            .ok_or(PublishError::UnknownRendition(level))?;
        adapter.discontinuity();
        Ok(())
    }

    /// Stop publishing entirely: owner-level teardown, never a budget decision.
    pub fn revoke(&mut self) {
        for adapter in &mut self.adapters {
            adapter.revoke();
        }
    }
}
