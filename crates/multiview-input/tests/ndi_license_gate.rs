//! Acceptance test for the NDI® runtime license gate on the **ingest** side
//! (ADR-0008 §7.5) — the receive mirror of
//! `multiview-output/tests/ndi_license_gate.rs`.
//!
//! With the license **not accepted**, no [`NdiProducer`] is constructed and the
//! receive seam is **never sampled** — a typed refusal, never a panic, never a
//! started source (the output-clock invariant is untouched; the tile degrades).
//! With acceptance, the producer opens and samples. The gate is enforced **by
//! construction**: the producer's start/constructor require an accepted
//! [`NdiLicense`], and the only way to obtain one is through an audited acceptance
//! record (who/when).
#![cfg(feature = "ndi")]
#![allow(
    // reason: integration test; the strict workspace lints are relaxed for
    // `tests/` per CLAUDE.md.
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_input::ndi::convert::ReceivedVideoFrame;
use multiview_input::ndi::license::LicenseAcceptance;
use multiview_input::ndi::receiver::{
    FakeNdiReceiver, NdiReceiver, NdiRecvError, NdiRecvFourCc, ReceivedFrame,
};
use multiview_input::ndi::{NdiLicense, NdiLicenseError, NdiProducer};
use multiview_input::source::FrameProducer;

/// A complete, audited acceptance (who/when present).
fn complete_audit() -> LicenseAcceptance {
    LicenseAcceptance {
        accepted_by: "operator@example".to_owned(),
        accepted_at: "2026-06-06T00:00:00Z".to_owned(),
    }
}

/// An accepted guard for the by-construction tests.
fn accepted() -> NdiLicense {
    NdiLicense::accept(complete_audit()).expect("a complete acceptance must be accepted")
}

/// A 2x2 UYVY frame (one UYVY group per row), reused as scripted receive content.
fn uyvy_2x2() -> ReceivedVideoFrame {
    let data = vec![
        100, 10, 200, 20, // row 0
        110, 30, 210, 40, // row 1
    ];
    ReceivedVideoFrame::new(2, 2, NdiRecvFourCc::Uyvy, 2 * 2, data).unwrap()
}

/// A receive seam that counts how many times it was sampled. Proving the count is
/// **zero** after a refused start is the ingest analogue of the output gate test's
/// `api.created == None`: a refused source never starts receiving. `Send` (an
/// `Arc<AtomicUsize>`, not `Rc`) so it satisfies `Box<dyn NdiReceiver + Send>`.
struct CountingReceiver {
    frames: std::collections::VecDeque<ReceivedFrame>,
    receives: Arc<AtomicUsize>,
}

impl CountingReceiver {
    fn new(frames: Vec<ReceivedFrame>, receives: Arc<AtomicUsize>) -> Self {
        Self {
            frames: frames.into(),
            receives,
        }
    }
}

impl NdiReceiver for CountingReceiver {
    fn receive(&mut self) -> Result<ReceivedFrame, NdiRecvError> {
        self.receives.fetch_add(1, Ordering::Relaxed);
        Ok(self.frames.pop_front().unwrap_or(ReceivedFrame::None))
    }
}

#[test]
fn unaccepted_setting_refuses_and_never_starts_receiving() {
    // `[system.ndi] accept_license = false`: a typed refusal, and — crucially — no
    // producer is constructed, so the receiver is never sampled (no frames flow).
    let receives = Arc::new(AtomicUsize::new(0));
    let recv = CountingReceiver::new(
        vec![
            ReceivedFrame::Video(uyvy_2x2()),
            ReceivedFrame::Video(uyvy_2x2()),
        ],
        Arc::clone(&receives),
    );

    let err = NdiProducer::start(false, complete_audit(), Box::new(recv))
        .expect_err("accept_license=false must be refused");
    assert_eq!(err, NdiLicenseError::NotAccepted);
    assert_eq!(
        receives.load(Ordering::Relaxed),
        0,
        "an unaccepted source must never sample the receive seam (no frames flow)"
    );
}

#[test]
fn accepted_setting_starts_and_samples() {
    // `[system.ndi] accept_license = true` with complete audit: the gate opens, the
    // producer is built, and it samples the receiver, and the audit record (who/when)
    // is reachable for the audit log / export.
    let mut producer = NdiProducer::start(
        true,
        complete_audit(),
        Box::new(FakeNdiReceiver::with_frames(vec![ReceivedFrame::Video(
            uyvy_2x2(),
        )])),
    )
    .expect("an accepted + audited setting must open the producer");

    let frame = producer
        .next_frame()
        .expect("sampling does not fault")
        .expect("the scripted frame is sampled");
    assert_eq!(frame.meta.width, 2);
    assert_eq!(frame.meta.height, 2);
    assert_eq!(
        producer.license().acceptance().accepted_by,
        "operator@example",
        "the audit record is reachable for export"
    );
}

#[test]
fn incomplete_acceptance_is_refused_not_panicked() {
    // accept_license = true but a blank audit field (who/when) is refused with the
    // typed IncompleteAcceptance — never a panic, and no producer is constructed.
    for acceptance in [
        LicenseAcceptance {
            accepted_by: "   ".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        },
        LicenseAcceptance {
            accepted_by: "ops".to_owned(),
            accepted_at: String::new(),
        },
    ] {
        let receives = Arc::new(AtomicUsize::new(0));
        let recv = CountingReceiver::new(
            vec![ReceivedFrame::Video(uyvy_2x2())],
            Arc::clone(&receives),
        );
        let err = NdiProducer::start(true, acceptance, Box::new(recv))
            .expect_err("a blank audit field must be refused");
        assert!(matches!(err, NdiLicenseError::IncompleteAcceptance { .. }));
        assert_eq!(
            receives.load(Ordering::Relaxed),
            0,
            "an incomplete acceptance must never sample the receive seam"
        );
    }
}

#[test]
fn refusal_is_prompt_and_never_blocks() {
    // Invariant #1: a refusal must be returned at once — never a blocking wait that
    // could extend the engine prime-wait.
    let receives = Arc::new(AtomicUsize::new(0));
    let recv = CountingReceiver::new(
        vec![ReceivedFrame::Video(uyvy_2x2())],
        Arc::clone(&receives),
    );
    let t = Instant::now();
    let refused = NdiProducer::start(false, complete_audit(), Box::new(recv));
    assert!(refused.is_err());
    assert!(
        t.elapsed() < Duration::from_millis(50),
        "the license refusal must be prompt — never blocks (invariant #1)"
    );

    // And the gate does not change the never-block contract on the accepted path: a
    // quiet receive (no frame this instant) is `Ok(None)`, returned promptly.
    let mut producer = NdiProducer::start(
        true,
        complete_audit(),
        Box::new(FakeNdiReceiver::with_frames(vec![ReceivedFrame::None])),
    )
    .expect("accepted setting opens the producer");
    let t = Instant::now();
    assert!(producer.next_frame().expect("no fault").is_none());
    assert!(
        t.elapsed() < Duration::from_millis(50),
        "a quiet sample must return Ok(None) promptly — never blocks"
    );
}

#[test]
fn new_requires_a_license_by_construction_and_exposes_audit() {
    // The only producer constructor takes an accepted `NdiLicense` by value — there
    // is no ungated `NdiProducer::new(receiver)` path (mirrors `NdiOutput::new`).
    let producer = NdiProducer::new(accepted(), Box::new(FakeNdiReceiver::with_frames(vec![])));
    assert_eq!(
        producer.license().acceptance().accepted_by,
        "operator@example"
    );
    assert_eq!(
        producer.license().acceptance().accepted_at,
        "2026-06-06T00:00:00Z"
    );
}

/// The license refusal is **orthogonal to the runtime-availability axis**: even on
/// a host where the NDI runtime loads, an unaccepted source is refused and never
/// receives. Built only with the SDK-binding feature so it documents the contract
/// on the box that can actually load a runtime; the assertion itself is pure (the
/// gate refuses before any runtime/receiver interaction), so it is not `#[ignore]`d.
#[cfg(feature = "ndi-bindings")]
mod bindings_unaccepted {
    use super::{complete_audit, uyvy_2x2, CountingReceiver};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use multiview_input::ndi::receiver::ReceivedFrame;
    use multiview_input::ndi::{NdiCapability, NdiLicenseError, NdiProducer};

    #[test]
    fn refusal_wins_over_runtime_probe() {
        // The runtime may well be Available on the SDK box; the license axis is
        // independent and must still refuse.
        let _ = NdiCapability::probe();
        let receives = Arc::new(AtomicUsize::new(0));
        let recv = CountingReceiver::new(
            vec![ReceivedFrame::Video(uyvy_2x2())],
            Arc::clone(&receives),
        );
        let err = NdiProducer::start(false, complete_audit(), Box::new(recv))
            .expect_err("an unaccepted source is refused regardless of runtime availability");
        assert_eq!(err, NdiLicenseError::NotAccepted);
        assert_eq!(receives.load(Ordering::Relaxed), 0);
    }
}
