//! Caption-presence probe: a pure state machine over the subtitle path that
//! raises a caption-loss alarm when no caption activity is seen within a
//! configured timeout, and clears when activity resumes. Driven only by an
//! injected media clock — never by a live decode.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::alarm::{AlarmKind, PerceivedSeverity};
use multiview_core::time::MediaTime;
use multiview_overlay::caption_probe::{CaptionPresence, CaptionProbe};

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n * 1_000_000)
}

#[test]
fn starts_present_within_timeout() {
    // A fresh probe seen at t=0 is Present until the timeout elapses.
    let probe = CaptionProbe::new(ms(2000));
    assert_eq!(probe.presence(), CaptionPresence::Present);
    assert!(!probe.severity().is_active());
    assert_eq!(probe.severity(), PerceivedSeverity::Cleared);
}

#[test]
fn raises_caption_loss_after_timeout_without_activity() {
    let mut probe = CaptionProbe::new(ms(2000));
    probe.observe_caption(ms(0));
    // Still within the 2s window at t=1999ms.
    probe.tick(ms(1999));
    assert_eq!(probe.presence(), CaptionPresence::Present);
    // At/after the deadline with no new activity -> Lost.
    probe.tick(ms(2000));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
    assert_eq!(probe.severity(), PerceivedSeverity::Minor);
    assert_eq!(probe.alarm_kind(), AlarmKind::CaptionLoss);
}

#[test]
fn activity_resets_the_timeout_window() {
    let mut probe = CaptionProbe::new(ms(2000));
    probe.observe_caption(ms(0));
    probe.tick(ms(1500));
    assert_eq!(probe.presence(), CaptionPresence::Present);
    // New caption activity at 1500 pushes the deadline to 3500.
    probe.observe_caption(ms(1500));
    probe.tick(ms(3499));
    assert_eq!(probe.presence(), CaptionPresence::Present);
    probe.tick(ms(3500));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
}

#[test]
fn activity_after_loss_recovers_to_present() {
    let mut probe = CaptionProbe::new(ms(1000));
    probe.observe_caption(ms(0));
    probe.tick(ms(1000));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
    // Captions resume.
    probe.observe_caption(ms(1200));
    assert_eq!(probe.presence(), CaptionPresence::Present);
    assert!(!probe.severity().is_active());
    probe.tick(ms(2199));
    assert_eq!(probe.presence(), CaptionPresence::Present);
    probe.tick(ms(2200));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
}

#[test]
fn tick_is_monotonic_safe_and_idempotent_when_lost() {
    let mut probe = CaptionProbe::new(ms(500));
    probe.observe_caption(ms(0));
    probe.tick(ms(500));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
    // Repeated/late ticks keep it Lost (no flapping, no panic).
    probe.tick(ms(10_000));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
}

#[test]
fn presence_label_is_text_not_colour() {
    // a11y: caption presence conveyed by text.
    assert_eq!(CaptionPresence::Present.label(), "captions present");
    assert_eq!(CaptionPresence::Lost.label(), "captions lost");
}

#[test]
fn never_observed_then_timeout_is_lost() {
    // A probe that never sees a caption: the deadline runs from construction
    // time supplied at the first tick reference (t0 = 0 by construction).
    let mut probe = CaptionProbe::new(ms(1000));
    probe.tick(ms(1000));
    assert_eq!(probe.presence(), CaptionPresence::Lost);
}
