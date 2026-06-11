//! Integration tests for the pipeline-stage traits and shared enums.
//!
//! These confirm the stage traits stay object-safe (usable behind `dyn`) and
//! pure-Rust (no native types in the signatures), and that the shared
//! `BackendKind` / `SourceState` enums behave as documented.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::traits::{
    Backend, BackendKind, Compositor, Decoder, Encoder, Sink, Source, SourceState,
};

struct FakeBackend {
    name: String,
}
impl Backend for FakeBackend {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Software
    }
}

struct FakeSource {
    id: String,
    state: SourceState,
}
impl Source for FakeSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn state(&self) -> SourceState {
        self.state
    }
}

struct FakeSink {
    id: String,
}
impl Sink for FakeSink {
    fn id(&self) -> &str {
        &self.id
    }
}

#[test]
fn backend_is_object_safe() {
    let b: Box<dyn Backend> = Box::new(FakeBackend {
        name: "software".to_owned(),
    });
    assert_eq!(b.name(), "software");
    assert_eq!(b.kind(), BackendKind::Software);
}

#[test]
fn source_is_object_safe_and_reports_state() {
    let s: Box<dyn Source> = Box::new(FakeSource {
        id: "cam-1".to_owned(),
        state: SourceState::Live,
    });
    assert_eq!(s.id(), "cam-1");
    assert_eq!(s.state(), SourceState::Live);
}

#[test]
fn sink_is_object_safe() {
    let s: Box<dyn Sink> = Box::new(FakeSink {
        id: "rtsp-out".to_owned(),
    });
    assert_eq!(s.id(), "rtsp-out");
}

#[test]
fn source_state_default_is_no_signal() {
    // A freshly declared, not-yet-connected tile is NO_SIGNAL until proven live.
    assert_eq!(SourceState::default(), SourceState::NoSignal);
}

#[test]
fn source_state_transitions_are_representable() {
    // The documented lifecycle LIVE -> STALE -> RECONNECTING -> NO_SIGNAL.
    let lifecycle = [
        SourceState::Live,
        SourceState::Stale,
        SourceState::Reconnecting,
        SourceState::NoSignal,
    ];
    // Distinct variants.
    for (i, a) in lifecycle.iter().enumerate() {
        for (j, b) in lifecycle.iter().enumerate() {
            assert_eq!(i == j, a == b);
        }
    }
}

#[test]
fn source_state_live_predicate() {
    assert!(SourceState::Live.is_live());
    assert!(!SourceState::Stale.is_live());
    assert!(!SourceState::NoSignal.is_live());
}

#[test]
fn backend_kind_variants_distinct() {
    let kinds = [
        BackendKind::Software,
        BackendKind::Cuda,
        BackendKind::VideoToolbox,
        BackendKind::Vaapi,
        BackendKind::Qsv,
        BackendKind::Wgpu,
        BackendKind::Metal,
    ];
    for (i, a) in kinds.iter().enumerate() {
        for (j, b) in kinds.iter().enumerate() {
            assert_eq!(i == j, a == b);
        }
    }
}

#[test]
fn backend_kind_software_is_default() {
    assert_eq!(BackendKind::default(), BackendKind::Software);
}

#[test]
fn stage_traits_surface_is_kind_only() {
    // The Decoder/Encoder/Compositor stage traits expose exactly `kind()`. The
    // scaffold `describe_*` metadata methods were removed (the GPU path supplies
    // geometry per `composite` call and nothing called them) — so a fake stage
    // compiles implementing only `kind()`, with no `describe_*` to satisfy.
    struct FakeDecoder;
    impl Decoder for FakeDecoder {
        fn kind(&self) -> BackendKind {
            BackendKind::Software
        }
    }
    struct FakeEncoder;
    impl Encoder for FakeEncoder {
        fn kind(&self) -> BackendKind {
            BackendKind::Software
        }
    }
    struct FakeCompositor;
    impl Compositor for FakeCompositor {
        fn kind(&self) -> BackendKind {
            BackendKind::Software
        }
    }
    assert_eq!(FakeDecoder.kind(), BackendKind::Software);
    assert_eq!(FakeEncoder.kind(), BackendKind::Software);
    assert_eq!(FakeCompositor.kind(), BackendKind::Software);
}
