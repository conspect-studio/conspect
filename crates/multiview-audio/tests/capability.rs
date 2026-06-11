//! AUD-7: per-output audio capability matrix (the audio half).
//!
//! The declarative routing schema lives in `multiview-config`; this crate owns
//! the machine-readable capability matrix that says, per output transport, how
//! many discrete tracks and which channel layouts are deliverable — so an
//! impossible selection (N discrete tracks on legacy RTMP, >2ch AAC over NDI)
//! is rejected (or honestly degraded) at config time rather than failing live.
//!
//! These checks reference the existing audio types (`ChannelLayout`,
//! `AudioFormat`) and never panic — they return a typed [`AudioError`].
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_audio::capability::{
    DiscreteTracks, OutputCapability, OutputTransport, TrackSupport,
};
use multiview_audio::{AudioError, ChannelLayout};

#[test]
fn ts_carries_many_discrete_tracks() {
    let cap = OutputCapability::for_transport(OutputTransport::MpegTs);
    assert_eq!(cap.track_support(), TrackSupport::Multiple);
    // MPEG-TS carries N simultaneous PIDs: four discrete tracks is fine.
    cap.validate_tracks(DiscreteTracks::new(4))
        .expect("TS carries N discrete tracks");
}

#[test]
fn rtsp_carries_many_discrete_tracks() {
    let cap = OutputCapability::for_transport(OutputTransport::Rtsp);
    assert_eq!(cap.track_support(), TrackSupport::Multiple);
    cap.validate_tracks(DiscreteTracks::new(3))
        .expect("RTSP carries N simultaneous subsessions");
}

#[test]
fn hls_is_select_one() {
    let cap = OutputCapability::for_transport(OutputTransport::Hls);
    assert_eq!(cap.track_support(), TrackSupport::SelectOne);
    // A single rendition is fine...
    cap.validate_tracks(DiscreteTracks::new(1))
        .expect("HLS carries one selectable rendition");
    // ...but asking for simultaneous multitrack delivery is not (it is
    // select-one): this is the designed-in asymmetry vs TS (brief §9.4).
    let err = cap
        .validate_tracks(DiscreteTracks::new(2))
        .expect_err("HLS must reject simultaneous multitrack");
    assert!(matches!(err, AudioError::InvalidFormat(_)), "got {err:?}");
}

#[test]
fn rtmp_default_endpoint_is_program_only() {
    // RTMP is capability-gated: the conservative default endpoint carries only
    // the mixed program bus (one track) and rejects N discrete tracks until an
    // Enhanced-RTMP-v2 endpoint negotiates multitrack.
    let cap = OutputCapability::for_transport(OutputTransport::Rtmp);
    assert_eq!(cap.track_support(), TrackSupport::SingleProgramOnly);
    cap.validate_tracks(DiscreteTracks::new(1))
        .expect("RTMP carries the mixed program bus");
    let err = cap
        .validate_tracks(DiscreteTracks::new(2))
        .expect_err("default RTMP must reject multitrack");
    assert!(matches!(err, AudioError::InvalidFormat(_)), "got {err:?}");
}

#[test]
fn ndi_is_channel_map_never_n_tracks() {
    // NDI is ONE multiplexed stream: it carries a channel-map, never selectable
    // tracks. Many *inputs* fold into channels of one sender.
    let cap = OutputCapability::for_transport(OutputTransport::Ndi);
    assert_eq!(cap.track_support(), TrackSupport::ChannelMap);
    // Validated as a channel map, not as discrete tracks: two discrete tracks
    // is rejected (NDI never selects tracks).
    let err = cap
        .validate_tracks(DiscreteTracks::new(2))
        .expect_err("NDI never carries N selectable tracks");
    assert!(matches!(err, AudioError::InvalidFormat(_)), "got {err:?}");
    // But a single program sender is fine.
    cap.validate_tracks(DiscreteTracks::new(1))
        .expect("NDI carries a single multiplexed sender");
}

#[test]
fn ndi_channel_map_validates_against_layout() {
    let cap = OutputCapability::for_transport(OutputTransport::Ndi);
    // NDI PCM is effectively unlimited channels; 5.1 maps fine.
    cap.validate_channel_map(ChannelLayout::FivePointOne)
        .expect("NDI PCM carries a 5.1 channel map");
}

#[test]
fn zero_tracks_is_always_rejected() {
    for transport in [
        OutputTransport::MpegTs,
        OutputTransport::Rtsp,
        OutputTransport::Hls,
        OutputTransport::Rtmp,
        OutputTransport::Ndi,
    ] {
        let cap = OutputCapability::for_transport(transport);
        let err = cap
            .validate_tracks(DiscreteTracks::new(0))
            .expect_err("an output with zero audio tracks is never valid");
        assert!(matches!(err, AudioError::InvalidFormat(_)), "got {err:?}");
    }
}
