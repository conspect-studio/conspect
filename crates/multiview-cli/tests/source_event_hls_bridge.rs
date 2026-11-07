#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_cli::{
    source_event_hls_bridge::channel,
    source_event_service::{ServiceLimits, SourceEventService},
};
use multiview_input::hls::MediaPlaylist;
use std::time::{Duration, Instant};

fn playlist(asset: bool) -> multiview_input::hls::PlaylistCueMetadata {
    let text = if asset {
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ASSET:CAID=0x4144\n#EXTINF:6,\na.ts\n"
    } else {
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\na.ts\n"
    };
    MediaPlaylist::parse_cue_metadata(text).unwrap()
}

#[test]
fn original_playlist_recurrence_rebinds_only_to_native_epoch() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let producer = service.register("s", 42).unwrap();
    let (mut observer, mut bridge) = channel("s".into(), 7, 8);
    observer.observe(&playlist(true));
    observer.observe(&playlist(false));
    observer.observe(&playlist(true));
    bridge.drain_into("s", 42, &producer);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let read = service.registry().snapshot("s").unwrap();
        if read.events.len() == 2 {
            assert!(read.events.iter().all(|event| event.key.epoch == 42));
            assert_ne!(read.events[0].key.event_id, read.events[1].key.event_id);
            assert!(read.events.iter().all(|event| matches!(
                event.key.origin,
                multiview_core::source_event::EventOrigin::VendorMarker { .. }
            )));
            break;
        }
        assert!(Instant::now() < deadline, "marker publication timed out");
        std::thread::yield_now();
    }
    observer.retire();
    // Retirement invalidates published evidence even if no next frame drains it.
    assert!(service.registry().snapshot("s").unwrap().degraded);
    bridge.drain_into("s", 42, &producer);
    // Fetch teardown never retires the shared native owner.
    assert!(service.registry().snapshot("s").is_some());
    drop(producer);
    service.shutdown().unwrap();
}

#[test]
fn early_overflow_and_retirement_survive_until_mapping_ready() {
    for retire in [false, true] {
        let service = SourceEventService::new(ServiceLimits::default()).unwrap();
        let producer = service.register("s", 42).unwrap();
        let (mut observer, mut bridge) = channel("s".into(), 7, 1);
        observer.observe(&playlist(true));
        if retire {
            observer.retire();
        } else {
            observer.observe(&playlist(false));
        }
        bridge.drain_into("s", 42, &producer);
        assert!(service.registry().snapshot("s").unwrap().degraded);
        if retire {
            assert!(service.registry().snapshot("s").unwrap().events.is_empty());
        }
        drop(producer);
        service.shutdown().unwrap();
    }
}

#[test]
fn replacement_epoch_never_receives_old_fetch_evidence() {
    let service = SourceEventService::new(ServiceLimits::default()).unwrap();
    let producer = service.register("s", 42).unwrap();
    let (mut observer, mut bridge) = channel("s".into(), 7, 8);
    bridge.drain_into("s", 42, &producer);
    observer.observe(&playlist(true));
    let replacement = service.register("s", 43).unwrap();
    bridge.drain_into("s", 43, &replacement);
    let read = service.registry().snapshot("s").unwrap();
    assert!(read.events.is_empty());
    assert!(read.degraded);
    drop(replacement);
    drop(producer);
    service.shutdown().unwrap();
}
