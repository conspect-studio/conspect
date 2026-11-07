#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_cli::hls_event_observer::{HlsEventObserver, HlsEvidenceSink};
use multiview_core::source_event::{EventBoundary, SourceEventObservation};
use multiview_input::hls::MediaPlaylist;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<(Vec<SourceEventObservation>, bool)>>);
impl HlsEvidenceSink for Sink {
    fn try_observe(&mut self, o: SourceEventObservation) -> bool {
        self.0.lock().unwrap().0.push(o);
        true
    }
    fn mark_degraded(&mut self) {
        self.0.lock().unwrap().1 = true;
    }
    fn retire(&mut self) {
        self.0.lock().unwrap().0.clear();
    }
}
fn snapshot(assets: &[&str]) -> multiview_input::hls::PlaylistCueMetadata {
    let mut text = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n".to_owned();
    for asset in assets {
        let _ = write!(text, "#EXT-X-ASSET:CAID={asset}\n#EXTINF:6,\nlive.ts\n");
    }
    MediaPlaylist::parse_cue_metadata(&text).unwrap()
}
#[test]
fn recurrence_and_independent_membership_never_mean_splice_start() {
    let sink = Sink::default();
    let mut observer = HlsEventObserver::new("s".into(), 7, sink.clone(), 8, 256);
    observer.observe(&snapshot(&["A", "B", "A"]));
    observer.observe(&snapshot(&["A", "B"])); // mixed ad/live has no timing meaning
    observer.observe(&snapshot(&["B"]));
    observer.observe(&snapshot(&["A", "B"]));
    let data = sink.0.lock().unwrap();
    assert_eq!(data.0.len(), 4);
    assert_eq!(data.0[2].boundary, EventBoundary::MarkerAbsent);
    assert_eq!(data.0[0].key, data.0[2].key);
    assert_ne!(data.0[0].key, data.0[3].key);
    assert!(data.0.iter().all(|o| matches!(
        o.boundary,
        EventBoundary::MarkerObserved | EventBoundary::MarkerAbsent
    )));
}
#[test]
fn invalid_and_capacity_snapshots_never_withdraw() {
    for kind in 0..3 {
        let sink = Sink::default();
        let mut observer = HlsEventObserver::new("s".into(), 7, sink.clone(), 1, 256);
        observer.observe(&snapshot(&["A"]));
        let mut invalid = snapshot(&[]);
        match kind {
            0 => invalid.rejected = 1,
            1 => invalid.dropped = 1,
            _ => invalid = snapshot(&["B", "C"]),
        }
        observer.observe(&invalid);
        observer.observe(&snapshot(&[]));
        assert!(observer.degraded());
        let data = sink.0.lock().unwrap();
        assert!(data.1);
        assert_eq!(data.0.len(), 1);
    }
}
#[test]
fn retired_service_epoch_cannot_publish() {
    use multiview_cli::source_event_service::{ServiceLimits, SourceEventService};
    let service = SourceEventService::new(ServiceLimits {
        queue_capacity: 0,
        ..Default::default()
    })
    .unwrap();
    let producer = service.register("s", 1).unwrap();
    let mut old = HlsEventObserver::new("s".into(), 1, producer, 4, 256);
    let current = service.register("s", 2).unwrap();
    old.observe(&snapshot(&["A"]));
    assert!(old.degraded());
    assert_eq!(service.registry().snapshot("s").unwrap().epoch, 2);
    assert!(service.registry().snapshot("s").unwrap().events.is_empty());
    drop(current);
    service.shutdown().unwrap();
}

#[test]
fn rejected_sink_latches_loss_without_later_absence() {
    struct Reject;
    impl HlsEvidenceSink for Reject {
        fn try_observe(&mut self, _: SourceEventObservation) -> bool {
            false
        }
        fn mark_degraded(&mut self) {}
        fn retire(&mut self) {}
    }
    let mut observer = HlsEventObserver::new("s".into(), 1, Reject, 4, 256);
    observer.observe(&snapshot(&["A"]));
    assert!(observer.degraded());
    observer.observe(&snapshot(&[]));
    assert!(observer.degraded());
}

#[test]
fn decoded_asset_label_and_raw_correlation_survive_absence_and_recurrence() {
    let sink = Sink::default();
    let mut observer = HlsEventObserver::new("s".into(), 7, sink.clone(), 8, 256);
    observer.observe(&snapshot(&["0x41442D31", "0x001B"]));
    observer.observe(&snapshot(&[]));
    observer.observe(&snapshot(&["0x41442D31"]));
    let data = sink.0.lock().unwrap();
    let printable: Vec<_> = data
        .0
        .iter()
        .filter(|o| o.correlation_id.as_deref() == Some("0x41442D31"))
        .collect();
    assert_eq!(printable.len(), 3);
    assert!(printable
        .iter()
        .all(|o| o.asset_id.as_deref() == Some("AD-1")));
    assert_eq!(printable[0].key, printable[1].key);
    assert_eq!(printable[1].boundary, EventBoundary::MarkerAbsent);
    assert_ne!(printable[0].key, printable[2].key);
    let opaque: Vec<_> = data
        .0
        .iter()
        .filter(|o| o.correlation_id.as_deref() == Some("0x001B"))
        .collect();
    assert_eq!(opaque.len(), 2);
    assert!(opaque
        .iter()
        .all(|o| o.asset_id.as_deref() == Some("0x001B")));
}

#[test]
fn daterange_is_retained_without_fake_timing() {
    let sink = Sink::default();
    let mut observer = HlsEventObserver::new("s".into(), 7, sink.clone(), 8, 256);
    let metadata = MediaPlaylist::parse_cue_metadata(
        "#EXTM3U\n#EXT-X-DATERANGE:ID=\"cue\",SCTE35-OUT=0xFC00\n#EXTINF:6,\na.ts\n",
    )
    .unwrap();
    assert_eq!(metadata.observations.len(), 1);
    observer.observe(&metadata);
    assert_eq!(observer.pending_date_ranges(), metadata.observations);
    assert_eq!(
        sink.0.lock().unwrap().0,
        Vec::<multiview_core::source_event::SourceEventObservation>::new()
    );
}
