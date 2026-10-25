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

/// Re-points per layer in the cross-instance test.
const XS_REPOINTS: u64 = 50_000;

/// A cue source that always reports one fixed cue whose text names its owner
/// and re-point counter (`"a:7"`).
struct Tagged(&'static str, u64);

impl CueSource for Tagged {
    fn active_at(&self, _now: MediaTime) -> Option<multiview_overlay::subtitle::Cue> {
        Some(multiview_overlay::subtitle::Cue {
            start: MediaTime::ZERO,
            end: at_secs(1),
            lines: vec![format!("{}:{}", self.0, self.1)],
        })
    }
}

/// What one `peek` of layer `a` may legally return: a `Tagged("a", n)` cue
/// with `n` in `0..=XS_REPOINTS`, never older than `prev`. `b`'s cue, no cue
/// (e.g. an `EmptyCueSource`), a malformed or multi-line cue, or an
/// out-of-range counter all fail (#134 review: no relative-only checks).
fn check_layer_a(cue: Option<multiview_overlay::subtitle::Cue>, prev: u64) -> Result<u64, String> {
    let cue = cue.ok_or("layer `a` sampled a foreign source with no cue")?;
    let [line] = cue.lines.as_slice() else {
        return Err(format!("layer `a` sampled a foreign cue {:?}", cue.lines));
    };
    let n: u64 = line
        .strip_prefix("a:")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("layer `a` sampled a foreign cue {line:?}"))?;
    if n > XS_REPOINTS {
        return Err(format!("layer `a` sampled a foreign cue {line:?} (range)"));
    }
    if n < prev {
        return Err(format!("source regressed: {n} < {prev}"));
    }
    Ok(n)
}

#[test]
fn layer_validator_rejects_foreign_cues() {
    let cue = |owner, n| Tagged(owner, n).active_at(MediaTime::ZERO);
    assert_eq!(check_layer_a(cue("a", 0), 0), Ok(0));
    assert_eq!(check_layer_a(cue("a", 9), 3), Ok(9));
    assert!(check_layer_a(cue("b", 9), 0).is_err(), "`b`'s cue");
    assert!(check_layer_a(None, 0).is_err(), "no cue");
    assert!(check_layer_a(cue("a", XS_REPOINTS + 1), 0).is_err());
    assert!(check_layer_a(cue("a", 3), 9).is_err(), "regression");
    let mut two = cue("a", 1);
    if let Some(c) = two.as_mut() {
        c.lines.push("a:2".to_owned());
    }
    assert!(check_layer_a(two, 0).is_err(), "multi-line cue");
}

/// Regression for #134 (upstream vorner/arc-swap#210): two independent layers
/// re-pointed in lock-step, so the source cell freed from `a` is reused for
/// `b`'s next one. Every `peek` of `a` must pass [`check_layer_a`].
#[test]
fn sample_never_reads_a_source_repointed_into_another_layer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let a = Arc::new(SubtitleLayer::new(Arc::new(Tagged("a", 0))));
    let b = Arc::new(SubtitleLayer::new(Arc::new(Tagged("b", 0))));
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        // `sample` needs `&mut`; `peek` is the same source load through `&self`.
        let (a, done) = (Arc::clone(&a), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut prev = 0;
            while !done.load(Ordering::Acquire) {
                prev =
                    check_layer_a(a.peek(MediaTime::ZERO), prev).unwrap_or_else(|e| panic!("{e}"));
            }
        })
    };
    for n in 1..=XS_REPOINTS {
        a.repoint(Arc::new(Tagged("a", n)));
        b.repoint(Arc::new(Tagged("b", n)));
    }
    done.store(true, Ordering::Release);
    reader.join().expect("reader panicked");
    assert_eq!(check_layer_a(a.peek(MediaTime::ZERO), 0), Ok(XS_REPOINTS));
}
