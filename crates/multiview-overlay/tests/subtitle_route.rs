//! Integration tests for the **per-layer subtitle cue re-point** mechanic
//! (RT-10a, decoupled-routing subtitle breakaway; ADR-0034 / ADR-0019).
//!
//! A subtitle/caption [`SubtitleLayer`] holds an active cue source (an
//! `Arc<dyn CueSource>`, i.e. another input's `CueStore`) and samples it per
//! output tick via `active_at(now)`. The layer can be **re-pointed** to a
//! different source's cue stream instantly (replace semantics); the re-point
//! takes effect on the next tick. At the seam, the previously-displayed cue is
//! **CLEAR**ed (hard cut, no cross-fade) so a stale/wide cue from the old source
//! can never persist or flash over the new one.
//!
//! REAL assertions (the three acceptance tests of RT-10a):
//! (a) re-pointing to another source shows the NEW source's cue at the next tick
//!     (sampled via `active_at`);
//! (b) the old source's in-flight cue is CLEARED at the seam — a renderer that
//!     latches the last cue must not carry it past the re-point;
//! (c) re-pointing to an empty/no-cue source shows nothing, not the old cue.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_core::time::MediaTime;
use multiview_overlay::subtitle::CueTrack;
use multiview_overlay::subtitle_route::{CueSource, EmptyCueSource, SubtitleLayer};

/// A media time at `secs` whole seconds.
fn at_secs(secs: i64) -> MediaTime {
    MediaTime::from_nanos(secs.saturating_mul(1_000_000_000))
}

/// A single-cue source spanning `[start_s, end_s)` carrying `text`, built by
/// parsing a tiny SRT document so the cue store under test is the real
/// [`CueTrack`] (which implements [`CueSource`]), not a bespoke stub.
fn srt_source(start: i64, end: i64, text: &str) -> Arc<dyn CueSource> {
    let doc = format!("1\n00:00:{start:02},000 --> 00:00:{end:02},000\n{text}\n");
    let track = CueTrack::parse_srt(&doc).expect("valid srt fixture");
    Arc::new(track)
}

/// The text of the cue a layer would display at `now` (its rendered lines joined),
/// or `None` when the layer shows nothing this tick.
fn displayed_text(layer: &mut SubtitleLayer, now: MediaTime) -> Option<String> {
    layer.sample(now).map(|cue| cue.text())
}

#[test]
fn cue_track_is_a_cue_source_sampled_by_active_at() {
    // A bare CueTrack samples exactly like the CaptionCueStore active_at window.
    let track = CueTrack::parse_srt("1\n00:00:01,000 --> 00:00:03,000\nhi\n").unwrap();
    assert_eq!(CueSource::active_at(&track, at_secs(0)), None);
    assert_eq!(
        CueSource::active_at(&track, at_secs(2)).map(|c| c.text()),
        Some("hi".to_owned())
    );
    assert_eq!(CueSource::active_at(&track, at_secs(3)), None);
}

#[test]
fn repoint_shows_the_new_sources_cue_at_the_next_tick() {
    // (a) Layer points at source A (cue "alpha" over 1..9s). At t=2s it shows A.
    let source_a = srt_source(1, 9, "alpha");
    let mut layer = SubtitleLayer::new(Arc::clone(&source_a));
    assert_eq!(
        displayed_text(&mut layer, at_secs(2)),
        Some("alpha".to_owned())
    );

    // Re-point to source B (cue "bravo" over 1..9s). The next tick (the seam,
    // t=3s) samples B via active_at and shows B's cue — the new source's cue at
    // the next tick.
    let source_b = srt_source(1, 9, "bravo");
    layer.repoint(Arc::clone(&source_b));
    assert_eq!(
        displayed_text(&mut layer, at_secs(3)),
        Some("bravo".to_owned())
    );
}

#[test]
fn the_old_sources_in_flight_cue_is_cleared_at_the_seam() {
    // (b) Source A has a WIDE cue "stale" over 0..100s — always active. The layer
    // latches the last-displayed cue (renderer stability), so at t=1s it shows it.
    let source_a = srt_source(0, 100, "stale");
    let mut layer = SubtitleLayer::new(Arc::clone(&source_a));
    assert_eq!(
        displayed_text(&mut layer, at_secs(1)),
        Some("stale".to_owned())
    );

    // Re-point to source B, which has NO cue active at the seam instant (its only
    // cue starts later, 50..60s). At t=2s the layer must CLEAR — the old wide cue
    // must not persist past the re-point even though the latch held it a tick ago.
    let source_b = srt_source(50, 60, "late");
    layer.repoint(Arc::clone(&source_b));
    assert_eq!(displayed_text(&mut layer, at_secs(2)), None);

    // And once B's own cue window is reached, it renders normally.
    assert_eq!(
        displayed_text(&mut layer, at_secs(55)),
        Some("late".to_owned())
    );
}

#[test]
fn repoint_to_an_empty_source_shows_nothing_not_the_old_cue() {
    // (c) Source A shows "alpha"; re-point to an EmptyCueSource (a spare input
    // carrying no caption track). The seam clears; nothing renders thereafter.
    let source_a = srt_source(0, 100, "alpha");
    let mut layer = SubtitleLayer::new(source_a);
    assert_eq!(
        displayed_text(&mut layer, at_secs(5)),
        Some("alpha".to_owned())
    );

    layer.repoint(Arc::new(EmptyCueSource));
    assert_eq!(displayed_text(&mut layer, at_secs(6)), None);
    assert_eq!(displayed_text(&mut layer, at_secs(7)), None);
}

#[test]
fn the_seam_is_a_hard_cut_not_a_cross_fade() {
    // The first sample after a re-point is flagged as a hard-cut seam so the
    // renderer clears-then-draws (no cross-fade for subtitles).
    let mut layer = SubtitleLayer::new(srt_source(0, 100, "a"));
    let _ = layer.sample(at_secs(1));
    assert!(!layer.last_was_seam(), "a steady tick is not a seam");

    layer.repoint(srt_source(0, 100, "b"));
    let _ = layer.sample(at_secs(2));
    assert!(
        layer.last_was_seam(),
        "the first tick after a re-point is the seam"
    );

    let _ = layer.sample(at_secs(3));
    assert!(!layer.last_was_seam(), "subsequent ticks are steady again");
}

#[test]
fn without_a_repoint_a_steady_layer_keeps_sampling_the_same_source() {
    // Sanity: no re-point means normal per-tick active_at sampling of one source.
    let mut layer = SubtitleLayer::new(srt_source(2, 4, "only"));
    assert_eq!(displayed_text(&mut layer, at_secs(1)), None);
    assert_eq!(
        displayed_text(&mut layer, at_secs(3)),
        Some("only".to_owned())
    );
    assert_eq!(displayed_text(&mut layer, at_secs(5)), None);
}
