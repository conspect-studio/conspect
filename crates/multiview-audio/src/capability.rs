//! The per-output audio **capability matrix** (ADR-R005).
//!
//! The declarative routing schema (which inputs feed the program bus, which
//! discrete tracks exist, what each output selects) lives in `multiview-config`.
//! This module owns the machine-readable answer to the *other* question: given
//! an output transport, how many discrete audio tracks and which channel layouts
//! can it actually deliver? The brief (§9.4, §10) is explicit that this must be
//! a "first-class data structure, not scattered conditionals", because the same
//! matrix gates the config validator **and** the `WebUI` matrix (AUD-8).
//!
//! Verified transport limits (ADR-R005 rationale):
//! - **MPEG-TS / SRT / RTSP** carry *N* simultaneous tracks (PIDs / subsessions).
//! - **HLS / DASH** are **select-one**: one rendition is delivered at a time.
//! - **RTMP** is capability-gated: the conservative default carries only the
//!   mixed program bus; Enhanced-RTMP-v2 multitrack is negotiated per endpoint
//!   (not assumed here).
//! - **NDI** is ONE multiplexed stream — a **channel map**, never selectable
//!   tracks (up to 255 Opus / effectively unlimited PCM channels).
//!
//! This is pure validation referencing [`crate::ChannelLayout`]; it never
//! panics — every rejection is a typed [`crate::AudioError`].

use crate::error::{AudioError, Result};
use crate::format::ChannelLayout;

/// An output transport, keyed for the capability matrix.
///
/// Distinct from `multiview-config`'s `Output` enum: this is the delivery-layer
/// axis the capability depends on, so two outputs sharing a transport share a
/// capability. SRT shares MPEG-TS's carriage (N PIDs), so it maps to
/// [`OutputTransport::MpegTs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutputTransport {
    /// MPEG-TS carriage (also SRT/RIST): N simultaneous PIDs.
    MpegTs,
    /// RTSP: N simultaneous `m=audio` subsessions.
    Rtsp,
    /// HLS / LL-HLS / DASH: select-one rendition.
    Hls,
    /// RTMP push: endpoint-gated; conservative default is program-bus-only.
    Rtmp,
    /// NDI: one multiplexed stream carrying a channel map.
    Ndi,
}

/// How a transport carries multiple audio tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrackSupport {
    /// Carries N discrete tracks simultaneously (TS/SRT/RTSP).
    Multiple,
    /// Carries one selectable track/rendition at a time (HLS/DASH).
    SelectOne,
    /// Carries only the single mixed program bus by default (RTMP, until an
    /// endpoint negotiates multitrack).
    SingleProgramOnly,
    /// Carries a channel map within one multiplexed stream, never selectable
    /// tracks (NDI).
    ChannelMap,
}

/// A requested number of discrete audio tracks for an output (newtype so a bare
/// integer cannot be passed where a track count is meant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiscreteTracks(u32);

impl DiscreteTracks {
    /// A request for `count` discrete tracks.
    #[must_use]
    pub const fn new(count: u32) -> Self {
        Self(count)
    }

    /// The requested track count.
    #[must_use]
    pub const fn count(self) -> u32 {
        self.0
    }
}

/// The capability of one output transport: how it carries tracks and the channel
/// ceilings it imposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutputCapability {
    transport: OutputTransport,
    track_support: TrackSupport,
    /// Maximum channels in a single carried stream (e.g. NDI AAC caps at 2;
    /// `None` ⇒ effectively unbounded, e.g. NDI PCM).
    max_channels_per_stream: Option<u32>,
}

impl OutputCapability {
    /// The capability of the given transport (ADR-R005 rationale, verified
    /// limits).
    #[must_use]
    pub const fn for_transport(transport: OutputTransport) -> Self {
        let (track_support, max_channels_per_stream) = match transport {
            OutputTransport::MpegTs | OutputTransport::Rtsp => (TrackSupport::Multiple, None),
            OutputTransport::Hls => (TrackSupport::SelectOne, None),
            OutputTransport::Rtmp => (TrackSupport::SingleProgramOnly, None),
            // NDI PCM carries effectively unlimited channels; the AAC-over-NDI
            // 2-channel cap applies only when AAC is selected, which this default
            // (PCM) capability does not impose.
            OutputTransport::Ndi => (TrackSupport::ChannelMap, None),
        };
        Self {
            transport,
            track_support,
            max_channels_per_stream,
        }
    }

    /// The transport this capability describes.
    #[must_use]
    pub const fn transport(self) -> OutputTransport {
        self.transport
    }

    /// How this transport carries tracks.
    #[must_use]
    pub const fn track_support(self) -> TrackSupport {
        self.track_support
    }

    /// The maximum channels in a single carried stream, if bounded.
    #[must_use]
    pub const fn max_channels_per_stream(self) -> Option<u32> {
        self.max_channels_per_stream
    }

    /// Validate that this transport can deliver `requested` discrete audio
    /// tracks.
    ///
    /// Zero tracks is always invalid (an output must carry *some* audio).
    /// Otherwise:
    /// - [`TrackSupport::Multiple`] accepts any positive count.
    /// - [`TrackSupport::SelectOne`], [`TrackSupport::SingleProgramOnly`] and
    ///   [`TrackSupport::ChannelMap`] accept exactly one carried track (HLS
    ///   selects one rendition; RTMP default carries the program bus; NDI carries
    ///   one multiplexed sender). Asking for more is the designed-in asymmetry vs
    ///   TS (brief §9.4) and is rejected here so the config validator can degrade
    ///   honestly rather than promising multitrack the transport cannot deliver.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`] when the request exceeds what the
    /// transport delivers (or asks for zero tracks).
    pub fn validate_tracks(self, requested: DiscreteTracks) -> Result<()> {
        if requested.count() == 0 {
            return Err(AudioError::InvalidFormat(
                "an output must carry at least one audio track",
            ));
        }
        match self.track_support {
            TrackSupport::Multiple => Ok(()),
            TrackSupport::SelectOne => {
                if requested.count() > 1 {
                    Err(AudioError::InvalidFormat(
                        "this transport is select-one: it delivers a single rendition at a time, \
                         not simultaneous multitrack",
                    ))
                } else {
                    Ok(())
                }
            }
            TrackSupport::SingleProgramOnly => {
                if requested.count() > 1 {
                    Err(AudioError::InvalidFormat(
                        "this transport carries only the mixed program bus by default; multitrack \
                         requires a negotiated Enhanced-RTMP-v2 endpoint",
                    ))
                } else {
                    Ok(())
                }
            }
            TrackSupport::ChannelMap => {
                if requested.count() > 1 {
                    Err(AudioError::InvalidFormat(
                        "this transport is one multiplexed stream (a channel map), never \
                         selectable tracks",
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Validate that this transport can carry the given `layout` as a channel
    /// map within a single stream (the relevant question for NDI and any
    /// channel-mapped transport).
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`] when the layout's channel count
    /// exceeds the transport's per-stream channel ceiling.
    pub fn validate_channel_map(self, layout: ChannelLayout) -> Result<()> {
        // `channel_count()` is a `usize`; widen to `u64` fallibly (it never
        // exceeds `u64` in practice, but the cast lint is denied workspace-wide).
        let channels = u64::try_from(layout.channel_count())
            .map_err(|_| AudioError::InvalidFormat("channel count does not fit a 64-bit width"))?;
        match self.max_channels_per_stream {
            Some(max) if channels > u64::from(max) => Err(AudioError::InvalidFormat(
                "this transport's single stream cannot carry that many channels",
            )),
            _ => Ok(()),
        }
    }
}
