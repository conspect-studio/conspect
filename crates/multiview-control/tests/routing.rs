//! RT-11 / ADR-0034 §10 — the **#11 classifier** (`/routing/plan`) and the
//! `/routing/{kind}/take` crosspoint endpoints.
//!
//! These tests pin the classifier's honesty at the edges (it is NOT a universal
//! "all in-program = Class-1"):
//!
//! * a VIDEO re-point onto an existing cell is **Class-1**;
//! * a VIDEO re-point onto a cold (not-yet-primed) target is **Reset-lite**;
//! * an AUDIO re-point onto the program bus is **Class-1** (the bus absorbs the
//!   source layout);
//! * an AUDIO breakaway onto a **discrete track** whose pinned channel layout
//!   differs from the source is **Class-2** (the property the brief mandates:
//!   such a breakaway is *not* reported as plain Class-1) — unless the operator
//!   confirms a coerced down/up-mix (Class-1-with-degradation);
//! * a SUBTITLE re-point onto an existing layer is **Class-1**; a new track set is
//!   **Class-2**.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::routing::StreamRef;
use multiview_control::routing::{
    classify, DestinationProfile, RouteClass, RouteRequest, RouteTarget,
};
use multiview_core::stream::{
    StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};

fn audio_desc(channels: u16, pid: u16) -> StreamDescriptor {
    StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Audio, pid),
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels,
            sample_rate: 48_000,
        },
    )
}

fn audio_inventory(channels: u16) -> StreamInventory {
    StreamInventory::from_streams(vec![audio_desc(channels, 300)]).with_input_id("cam-b")
}

fn video_inventory() -> StreamInventory {
    StreamInventory::from_streams(vec![StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Video, 256),
        StreamKind::Video,
        "h264",
        StreamDetail::Video {
            width: 1920,
            height: 1080,
            frame_rate: None,
        },
    )])
    .with_input_id("cam-b")
}

// ---------------------------------------------------------------------------
// VIDEO — existing cell vs cold target.
// ---------------------------------------------------------------------------

#[test]
fn video_onto_existing_cell_is_class1() {
    let req = RouteRequest {
        target: RouteTarget::VideoCell {
            cell: "c0".to_owned(),
        },
        source: StreamRef::best("cam-b", StreamKind::Video),
    };
    // The destination cell exists and its source is already primed/decoding.
    let dest = DestinationProfile::video_cell(true);
    let plan = classify(&req, &video_inventory(), &dest);
    assert_eq!(plan.class, RouteClass::Class1);
    assert!(!plan.coerced);
}

#[test]
fn video_onto_cold_target_is_reset_lite() {
    let req = RouteRequest {
        target: RouteTarget::VideoCell {
            cell: "c0".to_owned(),
        },
        source: StreamRef::best("cam-b", StreamKind::Video),
    };
    // Cold target: the source is not yet primed (a single IDR is needed).
    let dest = DestinationProfile::video_cell(false);
    let plan = classify(&req, &video_inventory(), &dest);
    assert_eq!(plan.class, RouteClass::ResetLite);
}

// ---------------------------------------------------------------------------
// AUDIO — program bus (always Class-1) vs discrete track (layout-sensitive).
// ---------------------------------------------------------------------------

#[test]
fn audio_onto_program_bus_is_class1() {
    let req = RouteRequest {
        target: RouteTarget::AudioProgramBus {
            channel: "prog".to_owned(),
        },
        source: StreamRef::best("cam-b", StreamKind::Audio),
    };
    // A 6-channel source onto the program bus: the bus resamples to its working
    // layout, so the source layout is absorbed — Class-1 regardless.
    let plan = classify(
        &req,
        &audio_inventory(6),
        &DestinationProfile::audio_program_bus(),
    );
    assert_eq!(plan.class, RouteClass::Class1);
    assert!(!plan.coerced);
}

#[test]
fn audio_breakaway_matching_layout_is_class1() {
    let req = RouteRequest {
        target: RouteTarget::AudioDiscreteTrack {
            track: "trk-5_1".to_owned(),
            pinned_channels: None,
        },
        source: StreamRef::best("cam-b", StreamKind::Audio),
    };
    // A 6-channel source onto a discrete track pinned to 6 channels: layouts
    // match, so it is a clean Class-1 hot re-route.
    let dest = DestinationProfile::audio_discrete_track(6);
    let plan = classify(&req, &audio_inventory(6), &dest);
    assert_eq!(plan.class, RouteClass::Class1);
    assert!(!plan.coerced);
}

/// The brief's mandated property: a breakaway whose source layout ≠ the pinned
/// discrete-track layout is **NOT** reported as plain Class-1.
#[test]
fn audio_breakaway_layout_mismatch_is_not_class1() {
    let req = RouteRequest {
        target: RouteTarget::AudioDiscreteTrack {
            track: "trk-5_1".to_owned(),
            pinned_channels: None,
        },
        source: StreamRef::best("cam-b", StreamKind::Audio),
    };
    // A STEREO (2ch) source onto a discrete track pinned to 5.1 (6ch): the mux
    // pinned the layout for the session, so this is a Class-2 reset (or an
    // operator-confirmed coerced down/up-mix) — never a plain Class-1.
    let dest = DestinationProfile::audio_discrete_track(6);
    let plan = classify(&req, &audio_inventory(2), &dest);
    assert_ne!(
        plan.class,
        RouteClass::Class1,
        "a layout-mismatch breakaway must not be plain Class-1"
    );
    assert_eq!(plan.class, RouteClass::Class2);
}

#[test]
fn audio_breakaway_layout_mismatch_with_coercion_is_class1_degraded() {
    let req = RouteRequest {
        target: RouteTarget::AudioDiscreteTrack {
            track: "trk-5_1".to_owned(),
            pinned_channels: None,
        },
        source: StreamRef::best("cam-b", StreamKind::Audio),
    };
    // The operator opts in to a coerced down/up-mix to the pinned layout: the
    // classifier reports Class-1 but flags the degradation.
    let dest = DestinationProfile::audio_discrete_track(6).coerce_to_pinned();
    let plan = classify(&req, &audio_inventory(2), &dest);
    assert_eq!(plan.class, RouteClass::Class1);
    assert!(plan.coerced, "a coerced mismatch must flag degradation");
}

// ---------------------------------------------------------------------------
// SUBTITLE — existing layer vs new track set.
// ---------------------------------------------------------------------------

#[test]
fn subtitle_onto_existing_layer_is_class1() {
    let req = RouteRequest {
        target: RouteTarget::SubtitleLayer {
            layer: "subs".to_owned(),
        },
        source: StreamRef::best("cam-c", StreamKind::Subtitle),
    };
    let inv = StreamInventory::from_streams(vec![StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Subtitle, 500),
        StreamKind::Subtitle,
        "dvbsub",
        StreamDetail::Subtitle { forced: false },
    )])
    .with_input_id("cam-c");
    let plan = classify(&req, &inv, &DestinationProfile::subtitle_layer(true));
    assert_eq!(plan.class, RouteClass::Class1);
}

#[test]
fn subtitle_onto_new_track_set_is_class2() {
    let req = RouteRequest {
        target: RouteTarget::SubtitleLayer {
            layer: "subs".to_owned(),
        },
        source: StreamRef::best("cam-c", StreamKind::Subtitle),
    };
    let inv = StreamInventory::from_streams(vec![StreamDescriptor::new(
        StableStreamId::from_ts_pid(StreamKind::Subtitle, 500),
        StreamKind::Subtitle,
        "dvbsub",
        StreamDetail::Subtitle { forced: false },
    )])
    .with_input_id("cam-c");
    // The destination requires a new passthrough track set (not an existing
    // layer) — Class-2 per the capability matrix (set CRUD).
    let plan = classify(&req, &inv, &DestinationProfile::subtitle_layer(false));
    assert_eq!(plan.class, RouteClass::Class2);
}

// ---------------------------------------------------------------------------
// OpenAPI mirror drift guards: the published *Doc schemas must round-trip the
// real config/control serde shapes byte-identically, or the contract lies.
// ---------------------------------------------------------------------------

#[cfg(feature = "openapi")]
#[test]
fn openapi_stream_ref_mirror_matches_the_config_serde_shape() {
    use multiview_config::routing::StreamSelector;
    use multiview_control::openapi_schemas::StreamRefDoc;

    let mut audio = StreamRef::best("cam-b", StreamKind::Audio);
    audio.selector = StreamSelector::language("eng".to_owned());
    let mut subtitle = StreamRef::best("cam-c", StreamKind::Subtitle);
    subtitle.selector = StreamSelector::index(2);
    for source in [StreamRef::best("cam-b", StreamKind::Video), audio, subtitle] {
        let core_json = serde_json::to_value(&source).unwrap();
        let doc: StreamRefDoc = serde_json::from_value(core_json.clone())
            .expect("the StreamRefDoc mirror parses the real StreamRef JSON");
        assert_eq!(
            core_json,
            serde_json::to_value(&doc).unwrap(),
            "the OpenAPI StreamRefDoc must match the config StreamRef serde shape"
        );
    }
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_route_target_mirror_matches_the_control_serde_shape() {
    use multiview_control::openapi_schemas::RouteTargetDoc;

    for target in [
        RouteTarget::VideoCell {
            cell: "c0".to_owned(),
        },
        RouteTarget::AudioProgramBus {
            channel: "prog".to_owned(),
        },
        RouteTarget::AudioDiscreteTrack {
            track: "t".to_owned(),
            pinned_channels: Some(6),
        },
        RouteTarget::SubtitleLayer {
            layer: "subs".to_owned(),
        },
    ] {
        let control_json = serde_json::to_value(&target).unwrap();
        let doc: RouteTargetDoc = serde_json::from_value(control_json.clone())
            .expect("the RouteTargetDoc mirror parses the real RouteTarget JSON");
        assert_eq!(
            control_json,
            serde_json::to_value(&doc).unwrap(),
            "the OpenAPI RouteTargetDoc must match the control RouteTarget serde shape"
        );
    }
}
