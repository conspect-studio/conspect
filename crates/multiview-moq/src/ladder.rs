//! Shared multi-rendition (ABR) ladder: one admitted encoded family per rendition.
//!
//! The output owner admits each rendition exactly once, from encoder output it already
//! produces. This module advertises those exact families in the broadcast catalog and
//! publishes them on one common clock and configuration generation. It never encodes,
//! never resizes a picture, never creates a per-viewer copy and never invents a quality:
//! a ladder that was admitted with a single rendition is *explicitly* not adaptive
//! ([`Adaptivity::NotAdaptive`]) rather than a fabricated higher tier.

use std::{collections::BTreeSet, fmt, str::FromStr};

use bytes::Bytes;
use moq_net::{Timescale, Timestamp};

/// Hard bound on video renditions in one shared ladder. Together with
/// [`MAX_AUDIO_RENDITIONS`] this bounds the rendered tracks published in a broadcast.
pub const MAX_VIDEO_RENDITIONS: usize = 8;
/// Hard bound on independent audio renditions in one shared ladder.
pub const MAX_AUDIO_RENDITIONS: usize = 4;
const MAX_BITRATE_BPS: u64 = 1_000_000_000;
const MAX_DIMENSION: u32 = 8192;
const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LadderError {
    /// No admitted rendition: nothing may be advertised.
    Empty,
    TooManyRenditions {
        video: usize,
        audio: usize,
    },
    DuplicateName(String),
    /// Two admitted renditions cost the same, so the uplink budget could not order them.
    DuplicateBitrate(u64),
    InvalidName(String),
    InvalidCodec(String),
    InvalidDimensions {
        width: u32,
        height: u32,
    },
    InvalidCadence {
        numerator: u32,
        denominator: u32,
    },
    InvalidBitrate(u64),
    InvalidSampleRate(u32),
    InvalidChannels(u32),
    InvalidClock(u64),
    /// Rungs in one ladder disagreed on the video codec. A lease authorizes one codec, and a
    /// fail-closed selector refuses a rung whose codec is not authorized, so a mixed ladder
    /// would silently lose rungs. Admit a mixed ladder explicitly instead.
    MixedCodecs {
        first: String,
        other: String,
    },
    /// A catalog (or descriptor) operation failed; the message is the library's own.
    Catalog(String),
}

impl fmt::Display for LadderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "shared ladder has no admitted rendition"),
            Self::TooManyRenditions { video, audio } => write!(
                f,
                "shared ladder exceeds bounds: {video} video / {audio} audio renditions"
            ),
            Self::DuplicateName(name) => write!(f, "duplicate rendition name {name:?}"),
            Self::DuplicateBitrate(bps) => {
                write!(f, "two renditions share the admitted bitrate {bps} bps")
            }
            Self::InvalidName(name) => write!(f, "invalid rendition name {name:?}"),
            Self::InvalidCodec(codec) => write!(f, "invalid exact codec string {codec:?}"),
            Self::InvalidDimensions { width, height } => {
                write!(f, "invalid coded dimensions {width}x{height}")
            }
            Self::InvalidCadence {
                numerator,
                denominator,
            } => write!(f, "invalid cadence {numerator}/{denominator}"),
            Self::InvalidBitrate(bps) => write!(f, "invalid admitted bitrate {bps} bps"),
            Self::InvalidSampleRate(rate) => write!(f, "invalid sample rate {rate}"),
            Self::InvalidChannels(channels) => write!(f, "invalid channel count {channels}"),
            Self::InvalidClock(wall) => write!(f, "invalid common clock wall {wall}"),
            Self::MixedCodecs { first, other } => write!(
                f,
                "one ladder admits one video codec per authorization, found {first} and {other}"
            ),
            Self::Catalog(error) => write!(f, "catalog error: {error}"),
        }
    }
}

impl std::error::Error for LadderError {}

impl From<hang::Error> for LadderError {
    fn from(error: hang::Error) -> Self {
        Self::Catalog(error.to_string())
    }
}

/// Validate a rendition/track name: short, non-empty and limited to characters that are
/// safe in every catalog/player path, so the ladder descriptor never needs JSON escaping.
fn validate_name(name: &str) -> Result<(), LadderError> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(LadderError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// One admitted shared video rendition, as produced once by the output owner.
///
/// Codec, dimensions, cadence and bitrate are carried *exactly* as admitted: the catalog
/// and the ladder descriptor restate these values verbatim, so a consumer's capability
/// check and the uplink budget both work from the real encoder output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenditionSpec {
    name: String,
    codec: String,
    coded_width: u32,
    coded_height: u32,
    cadence_numerator: u32,
    cadence_denominator: u32,
    bitrate_bps: u64,
    description: Option<bytes::Bytes>,
}

impl RenditionSpec {
    /// Admit one existing encoder output. Never synthesizes a missing quality.
    ///
    /// The codec must be a valid **WebCodecs** codec string (H.264, HEVC, VP8, VP9 or AV1).
    /// Decodability is a per-consumer question — Safari and OS/Chrome with hardware support do
    /// decode HEVC — so the server advertises the valid descriptor and the client decides with
    /// `VideoDecoder.isConfigSupported`. A non-WebCodecs codec such as MPEG-2 is refused here.
    pub fn new(
        name: impl Into<String>,
        codec: impl Into<String>,
        coded_width: u32,
        coded_height: u32,
        cadence_numerator: u32,
        cadence_denominator: u32,
        bitrate_bps: u64,
    ) -> Result<Self, LadderError> {
        let name = name.into();
        validate_name(&name)?;
        let codec = codec.into();
        let parsed = hang::catalog::VideoCodec::from_str(&codec)
            .map_err(|_| LadderError::InvalidCodec(codec.clone()))?;
        if matches!(parsed, hang::catalog::VideoCodec::Unknown(_)) {
            return Err(LadderError::InvalidCodec(codec));
        }
        if coded_width < 2
            || coded_height < 2
            || coded_width > MAX_DIMENSION
            || coded_height > MAX_DIMENSION
            || !coded_width.is_multiple_of(2)
            || !coded_height.is_multiple_of(2)
        {
            return Err(LadderError::InvalidDimensions {
                width: coded_width,
                height: coded_height,
            });
        }
        // Exact broadcast cadences include 30000/1001; bound the ratio, not the numerator.
        if cadence_numerator == 0
            || cadence_denominator == 0
            || cadence_denominator > 1_001
            || cadence_numerator > 1_000_000
            || f64::from(cadence_numerator) / f64::from(cadence_denominator) > 240.0
        {
            return Err(LadderError::InvalidCadence {
                numerator: cadence_numerator,
                denominator: cadence_denominator,
            });
        }
        if bitrate_bps == 0 || bitrate_bps > MAX_BITRATE_BPS {
            return Err(LadderError::InvalidBitrate(bitrate_bps));
        }
        Ok(Self {
            name,
            codec,
            coded_width,
            coded_height,
            cadence_numerator,
            cadence_denominator,
            bitrate_bps,
            description: None,
        })
    }

    /// Preserve the opened codec initialization record. Hang serializes these
    /// bytes as hex; the HTTP grant independently encodes the same bytes as base64.
    pub fn with_description(mut self, description: Option<bytes::Bytes>) -> Self {
        self.description = description;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn codec(&self) -> &str {
        &self.codec
    }
    pub fn coded_width(&self) -> u32 {
        self.coded_width
    }
    pub fn coded_height(&self) -> u32 {
        self.coded_height
    }
    /// The exact admitted cadence as `(numerator, denominator)`, e.g. `(30000, 1001)`.
    pub fn cadence(&self) -> (u32, u32) {
        (self.cadence_numerator, self.cadence_denominator)
    }
    /// The admitted cadence as a frame rate, for the catalog's `framerate` field.
    pub fn framerate(&self) -> f64 {
        f64::from(self.cadence_numerator) / f64::from(self.cadence_denominator)
    }
    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }
    fn sort_key(&self) -> (u64, u64) {
        (
            self.bitrate_bps,
            u64::from(self.coded_width) * u64::from(self.coded_height),
        )
    }
}

/// One independent audio rendition. Audio is published once per broadcast, outside the
/// video ladder: selecting a different video rendition never re-encodes or re-publishes
/// audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSpec {
    name: String,
    codec: String,
    sample_rate: u32,
    channels: u32,
    bitrate_bps: u64,
    description: Option<bytes::Bytes>,
}

impl AudioSpec {
    pub fn new(
        name: impl Into<String>,
        codec: impl Into<String>,
        sample_rate: u32,
        channels: u32,
        bitrate_bps: u64,
    ) -> Result<Self, LadderError> {
        let name = name.into();
        validate_name(&name)?;
        let codec = codec.into();
        if let hang::catalog::AudioCodec::Unknown(_) =
            hang::catalog::AudioCodec::from_str(&codec)
                .map_err(|_| LadderError::InvalidCodec(codec.clone()))?
        {
            return Err(LadderError::InvalidCodec(codec));
        }
        if !(8_000..=384_000).contains(&sample_rate) {
            return Err(LadderError::InvalidSampleRate(sample_rate));
        }
        if !(1..=8).contains(&channels) {
            return Err(LadderError::InvalidChannels(channels));
        }
        if bitrate_bps == 0 || bitrate_bps > MAX_BITRATE_BPS {
            return Err(LadderError::InvalidBitrate(bitrate_bps));
        }
        Ok(Self {
            name,
            codec,
            sample_rate,
            channels,
            bitrate_bps,
            description: None,
        })
    }

    /// Preserve the opened codec initialization record. Hang serializes these
    /// bytes as hex; the HTTP grant independently encodes the same bytes as base64.
    pub fn with_description(mut self, description: Option<bytes::Bytes>) -> Self {
        self.description = description;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn codec(&self) -> &str {
        &self.codec
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u32 {
        self.channels
    }
    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }
    /// Always true: audio is an independent track, not a per-video-rendition copy.
    pub const fn independent(&self) -> bool {
        true
    }
}

/// Whether the publisher can actually offer more than one admitted quality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adaptivity {
    /// At least two admitted renditions: the consumer may select between them.
    Adaptive,
    /// Only one admitted rendition. Stated explicitly so no consumer treats a
    /// single-quality publication as an adaptive ladder.
    NotAdaptive { reason: NotAdaptiveReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotAdaptiveReason {
    /// The output owner admitted exactly one rendition; the publisher does not
    /// synthesize the missing qualities.
    SingleRendition,
}

impl NotAdaptiveReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SingleRendition => "single_admitted_rendition_no_synthesized_quality",
        }
    }
}

/// The admitted shared rendition family: exact specs, one configuration generation and one
/// common clock for every video track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedLadder {
    generation: u64,
    clock_wall_us: u64,
    renditions: Vec<RenditionSpec>,
    audio: Vec<AudioSpec>,
}

/// Whether one authorization may cover rungs with different video codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodecPolicy {
    /// One codec for the whole ladder: what a one-codec media lease authorizes, and what a
    /// fail-closed consumer selector expects.
    Single,
    /// Several codecs, admitted only when the authorization carries a codec set.
    Mixed,
}

impl SharedLadder {
    /// Admit a shared family. Renditions are ordered by rising admitted cost, which is the
    /// order the uplink budget's levels use.
    ///
    /// Every rung must share one video codec: the media lease authorizes one codec, and a
    /// fail-closed selector drops a rung whose codec is unauthorized, so a mixed ladder would
    /// silently lose rungs. Use [`SharedLadder::admit_multi_codec`] when the authorization
    /// really does carry a codec set.
    pub fn admit(
        generation: u64,
        renditions: Vec<RenditionSpec>,
        audio: Vec<AudioSpec>,
        clock_wall_us: u64,
    ) -> Result<Self, LadderError> {
        Self::admit_with_codecs(
            generation,
            renditions,
            audio,
            clock_wall_us,
            CodecPolicy::Single,
        )
    }

    /// Admit a ladder whose authorization carries several codecs.
    pub fn admit_multi_codec(
        generation: u64,
        renditions: Vec<RenditionSpec>,
        audio: Vec<AudioSpec>,
        clock_wall_us: u64,
    ) -> Result<Self, LadderError> {
        Self::admit_with_codecs(
            generation,
            renditions,
            audio,
            clock_wall_us,
            CodecPolicy::Mixed,
        )
    }

    fn admit_with_codecs(
        generation: u64,
        mut renditions: Vec<RenditionSpec>,
        mut audio: Vec<AudioSpec>,
        clock_wall_us: u64,
        codecs: CodecPolicy,
    ) -> Result<Self, LadderError> {
        if renditions.is_empty() {
            return Err(LadderError::Empty);
        }
        if renditions.len() > MAX_VIDEO_RENDITIONS || audio.len() > MAX_AUDIO_RENDITIONS {
            return Err(LadderError::TooManyRenditions {
                video: renditions.len(),
                audio: audio.len(),
            });
        }
        renditions.sort_by_key(RenditionSpec::sort_key);
        audio.sort_by(|left, right| left.name.cmp(&right.name));
        let mut names = BTreeSet::new();
        let mut previous: Option<RenditionSpec> = None;
        let mut authorized_codec: Option<String> = None;
        for rendition in &renditions {
            if !names.insert(rendition.name.clone()) {
                return Err(LadderError::DuplicateName(rendition.name.clone()));
            }
            match &authorized_codec {
                Some(first) if codecs == CodecPolicy::Single && *first != rendition.codec => {
                    return Err(LadderError::MixedCodecs {
                        first: first.clone(),
                        other: rendition.codec.clone(),
                    });
                }
                Some(_) => {}
                None => authorized_codec = Some(rendition.codec.clone()),
            }
            if let Some(previous) = &previous
                && previous.bitrate_bps == rendition.bitrate_bps
            {
                return Err(LadderError::DuplicateBitrate(rendition.bitrate_bps));
            }
            previous = Some(rendition.clone());
        }
        for spec in &audio {
            if !names.insert(spec.name.clone()) {
                return Err(LadderError::DuplicateName(spec.name.clone()));
            }
        }
        if Timestamp::new(clock_wall_us, Timescale::MICRO).is_err() {
            return Err(LadderError::InvalidClock(clock_wall_us));
        }
        Ok(Self {
            generation,
            clock_wall_us,
            renditions,
            audio,
        })
    }

    /// Admit exactly one rendition. The result reports [`Adaptivity::NotAdaptive`]; it never
    /// gains a second quality.
    pub fn single(
        generation: u64,
        rendition: RenditionSpec,
        audio: Vec<AudioSpec>,
        clock_wall_us: u64,
    ) -> Result<Self, LadderError> {
        Self::admit(generation, vec![rendition], audio, clock_wall_us)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// The common clock's PTS-zero wall time in microseconds since the MoQ epoch. Every
    /// video track in this ladder is stamped against it.
    pub fn clock_wall_us(&self) -> u64 {
        self.clock_wall_us
    }
    pub fn renditions(&self) -> &[RenditionSpec] {
        &self.renditions
    }
    pub fn audio(&self) -> &[AudioSpec] {
        &self.audio
    }
    pub fn len(&self) -> usize {
        self.renditions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.renditions.is_empty()
    }
    pub fn adaptivity(&self) -> Adaptivity {
        if self.renditions.len() > 1 {
            Adaptivity::Adaptive
        } else {
            Adaptivity::NotAdaptive {
                reason: NotAdaptiveReason::SingleRendition,
            }
        }
    }
    pub fn is_adaptive(&self) -> bool {
        matches!(self.adaptivity(), Adaptivity::Adaptive)
    }

    /// Track name for a ladder level.
    pub fn track_name(&self, level: usize) -> Option<&str> {
        self.renditions.get(level).map(RenditionSpec::name)
    }

    /// Video cost of a level in bits per second.
    pub fn level_cost_bps(&self, level: usize) -> Option<u64> {
        self.renditions.get(level).map(RenditionSpec::bitrate_bps)
    }

    /// Every level's admitted video cost, in level order.
    pub fn level_costs_bps(&self) -> Vec<u64> {
        self.renditions
            .iter()
            .map(RenditionSpec::bitrate_bps)
            .collect()
    }

    /// Audio bitrate that must be reserved from any uplink capacity before video levels are
    /// admitted. Zero when the publication carries no audio.
    pub fn reserved_audio_bps(&self) -> u64 {
        self.audio.iter().map(AudioSpec::bitrate_bps).sum()
    }

    /// The catalog for this ladder. `stalled` lists levels the publisher currently
    /// recommends avoiding; those tracks stay published and available, which is how an
    /// uplink shortfall is reported instead of silently dropping production output.
    pub fn catalog_with_stalled(
        &self,
        stalled: &[usize],
    ) -> Result<hang::catalog::Catalog<()>, LadderError> {
        let mut catalog = hang::catalog::Catalog::<()>::default();
        let wall = Timestamp::new(self.clock_wall_us, Timescale::MICRO)
            .map_err(|_| LadderError::InvalidClock(self.clock_wall_us))?;
        catalog.clock = Some(hang::catalog::Clock::new(wall)?);
        for (level, spec) in self.renditions.iter().enumerate() {
            let codec = hang::catalog::VideoCodec::from_str(&spec.codec)
                .map_err(|_| LadderError::InvalidCodec(spec.codec.clone()))?;
            let mut config = hang::catalog::VideoConfig::new(codec);
            config.description = spec.description.clone();
            config.coded_width = Some(spec.coded_width);
            config.coded_height = Some(spec.coded_height);
            config.bitrate = Some(spec.bitrate_bps);
            config.framerate = Some(spec.framerate());
            config.stalled = stalled.contains(&level).then_some(true);
            catalog.video.insert(&spec.name, config)?;
        }
        for spec in &self.audio {
            let codec = hang::catalog::AudioCodec::from_str(&spec.codec)
                .map_err(|_| LadderError::InvalidCodec(spec.codec.clone()))?;
            let mut config =
                hang::catalog::AudioConfig::new(codec, spec.sample_rate, spec.channels);
            config.description = spec.description.clone();
            config.bitrate = Some(spec.bitrate_bps);
            catalog.audio.insert(&spec.name, config)?;
        }
        catalog.check_renditions()?;
        Ok(catalog)
    }

    pub fn catalog(&self) -> Result<hang::catalog::Catalog<()>, LadderError> {
        self.catalog_with_stalled(&[])
    }

    pub fn catalog_json(&self) -> Result<Bytes, LadderError> {
        Ok(Bytes::from(self.catalog()?.to_vec()?))
    }

    /// The `ladder.json` descriptor: the exact admitted families, the one configuration
    /// generation, the common clock, the explicit adaptivity verdict and the reserved audio
    /// cost. Consumers (browser ABR selector, relay verification) read this beside
    /// `catalog.json`; the media catalog itself stays standard `hang`.
    pub fn descriptor_json(&self) -> Bytes {
        let adaptive = match self.adaptivity() {
            Adaptivity::Adaptive => true,
            Adaptivity::NotAdaptive { .. } => false,
        };
        let reason = match self.adaptivity() {
            Adaptivity::Adaptive => serde_json::Value::Null,
            Adaptivity::NotAdaptive { reason } => serde_json::Value::String(reason.as_str().into()),
        };
        let video = self
            .renditions
            .iter()
            .enumerate()
            .map(|(level, spec)| {
                let (numerator, denominator) = spec.cadence();
                serde_json::json!({
                    "level": level,
                    "name": spec.name,
                    "codec": spec.codec,
                    "codedWidth": spec.coded_width,
                    "codedHeight": spec.coded_height,
                    "cadence": { "numerator": numerator, "denominator": denominator },
                    "bitrateBps": spec.bitrate_bps,
                })
            })
            .collect::<Vec<_>>();
        let audio = self
            .audio
            .iter()
            .map(|spec| {
                serde_json::json!({
                    "name": spec.name,
                    "codec": spec.codec,
                    "sampleRate": spec.sample_rate,
                    "channels": spec.channels,
                    "bitrateBps": spec.bitrate_bps,
                })
            })
            .collect::<Vec<_>>();
        let descriptor = serde_json::json!({
            "version": 1,
            "generation": self.generation,
            "clock": { "wallMicros": self.clock_wall_us, "timescale": 1_000_000 },
            "adaptive": adaptive,
            "notAdaptiveReason": reason,
            "source": "admitted_shared_encoder_output",
            "cloudTranscodeRequired": false,
            "clientSupportCheck": "VideoDecoder.isConfigSupported",
            "audio": {
                "independent": true,
                "reservedBitrateBps": self.reserved_audio_bps(),
                "renditions": audio,
            },
            "video": {
                "codecs": self
                    .renditions
                    .iter()
                    .map(RenditionSpec::codec)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>(),
                "renditions": video,
            },
        });
        Bytes::from(serde_json::to_vec(&descriptor).expect("descriptor is serializable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendition(name: &str, bitrate_bps: u64) -> RenditionSpec {
        RenditionSpec::new(name, "vp8", 640, 360, 30, 1, bitrate_bps).unwrap()
    }

    #[test]
    fn decoder_description_roundtrips_as_hang_hex() {
        let description = bytes::Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff]);
        let spec = RenditionSpec::new("video", "avc1.64001f", 640, 360, 30, 1, 500_000)
            .unwrap()
            .with_description(Some(description.clone()));
        let ladder = SharedLadder::admit(5, vec![spec], vec![], 1).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&ladder.catalog_json().unwrap()).unwrap();
        assert!(json.to_string().contains("0164001fff"));
        let parsed: hang::catalog::Catalog<()> = serde_json::from_value(json).unwrap();
        let serialized = serde_json::to_value(parsed).unwrap();
        assert!(serialized.to_string().contains("0164001fff"));
    }

    #[test]
    fn rejects_fabricated_and_invalid_specs() {
        assert!(RenditionSpec::new("", "vp8", 640, 360, 30, 1, 1).is_err());
        assert!(RenditionSpec::new("a b", "vp8", 640, 360, 30, 1, 1).is_err());
        assert!(RenditionSpec::new("v", "made-up", 640, 360, 30, 1, 1).is_err());
        assert!(RenditionSpec::new("v", "vp8", 641, 360, 30, 1, 1).is_err());
        assert!(RenditionSpec::new("v", "vp8", 640, 360, 1, 0, 1).is_err());
        assert!(RenditionSpec::new("v", "vp8", 640, 360, 30, 1, 0).is_err());
        assert!(SharedLadder::admit(1, vec![], vec![], 0).is_err());
        let duplicates = vec![rendition("a", 1_000), rendition("a", 2_000)];
        assert_eq!(
            SharedLadder::admit(1, duplicates, vec![], 0),
            Err(LadderError::DuplicateName("a".into()))
        );
        let equal = vec![rendition("a", 1_000), rendition("b", 1_000)];
        assert_eq!(
            SharedLadder::admit(1, equal, vec![], 0),
            Err(LadderError::DuplicateBitrate(1_000))
        );
    }

    #[test]
    fn single_rendition_is_explicitly_not_adaptive() {
        let ladder = SharedLadder::single(4, rendition("video", 800_000), vec![], 0).unwrap();
        assert!(!ladder.is_adaptive());
        assert_eq!(
            ladder.adaptivity(),
            Adaptivity::NotAdaptive {
                reason: NotAdaptiveReason::SingleRendition
            }
        );
        let descriptor: serde_json::Value =
            serde_json::from_slice(&ladder.descriptor_json()).unwrap();
        assert_eq!(descriptor["adaptive"], serde_json::Value::Bool(false));
        assert_eq!(
            descriptor["notAdaptiveReason"],
            serde_json::Value::String(NotAdaptiveReason::SingleRendition.as_str().into())
        );
        let catalog: hang::catalog::Catalog<()> =
            hang::catalog::Catalog::from_slice(&ladder.catalog_json().unwrap()).unwrap();
        assert_eq!(catalog.video.renditions.len(), 1);
    }
}
