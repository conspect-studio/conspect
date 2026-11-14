//! Subtitle slots on outputs (slice F #345, ADR-0110 §12.3): the off-clock
//! cue sampler that fills an HLS output's `WebVTT` renditions.
//!
//! Each subtitle slot of a running HLS output is patched to a source through
//! a per-output **route table** ([`SubtitleRoutes`], one lock-free
//! `SwapCell` load per tick; initialised from the slot's configured `source`,
//! re-pointed live by the patch API in part F2). Once per output tick, on the
//! **bake consumer** (never the output clock: invariant #1), every slot asks a
//! resolver for the source's active caption lines at the picture the
//! compositor selects for that tick, and [`SubtitleSlotSampler`] turns the
//! per-tick samples into cue *changes* on the output timeline
//! (`MediaTime::from_tick`): an **open** cue when text appears and the
//! **closed** cue when it changes or disappears. Cues go to the sink over the
//! bounded [`SubtitleFeed`] (`try_send`, drop-and-count: invariant #10).
//!
//! A re-point ([`repoint`], part F2) replaces the whole table (RCU) and never
//! waits on the sampler. It lands on one tick: the tick that first reads the new route
//! closes the old source's cue at that tick and may open the new source's
//! cue at the same tick.
//!
//! Pure Rust: the resolver (frame-store latch + caption cue store) is
//! supplied by the pipeline, so this module builds in every configuration.

use std::sync::Arc;

use multiview_core::time::{MediaTime, Rational};
use multiview_framestore::SwapCell;
use multiview_output::hls::{SubtitleCue, SubtitleFeed};

/// One output's subtitle route table: slot index → patched source id
/// (`None` = unpatched). Replaced whole (RCU) by a re-patch.
pub type SubtitleRoutes = Arc<SwapCell<Arc<[Option<String>]>>>;

/// A route table initialised from the configured slot sources.
#[must_use]
pub fn subtitle_routes(sources: Vec<Option<String>>) -> SubtitleRoutes {
    Arc::new(SwapCell::new(Arc::from(sources)))
}

/// Re-point slots of `routes` (part F2): a copy of the current table with
/// each `(slot, source)` applied, stored whole (wait-free for the sampler,
/// which reads either the old or the new table, never a mix). `Err(slot)`
/// for a slot the table does not have; nothing is stored then. Concurrent
/// re-points of ONE table must be serialized by the caller (the adapter
/// holds a control-plane mutex; the sampler never takes it).
///
/// # Errors
/// The first slot beyond the table.
pub fn repoint(routes: &SubtitleRoutes, slots: &[(usize, Option<String>)]) -> Result<(), usize> {
    let mut next: Vec<Option<String>> = routes.load().to_vec();
    for (slot, source) in slots {
        let entry = next.get_mut(*slot).ok_or(*slot)?;
        entry.clone_from(source);
    }
    routes.store(Arc::from(next));
    Ok(())
}

/// The resolver the pipeline supplies: the active caption lines of source
/// `id` at output instant `now` (the picture the compositor latches for that
/// tick), or `None` (no caption store, no selected picture, no active text
/// cue).
pub type CueResolver = Box<dyn FnMut(&str, MediaTime) -> Option<Vec<String>> + Send>;

/// The cue currently shown on one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    source: String,
    lines: Vec<String>,
    start: MediaTime,
}

/// Turns one slot's per-tick samples into cue changes.
#[derive(Debug, Clone)]
pub struct SubtitleSlotSampler {
    slot: u8,
    shown: Option<Shown>,
}

impl SubtitleSlotSampler {
    /// A sampler for subtitle slot `slot`, showing nothing.
    #[must_use]
    pub const fn new(slot: u8) -> Self {
        Self { slot, shown: None }
    }

    /// Feed the sample of one tick at output instant `now`: the slot's
    /// current source (`None` = unpatched) and that source's active lines.
    /// Returns the cue changes in order: the closed previous cue (`start ..
    /// now`) when the text or the source changed, then the newly opened cue
    /// (`now ..` [`SubtitleCue::OPEN_END`]). Unchanged ⇒ nothing.
    pub fn sample(
        &mut self,
        now: MediaTime,
        source: Option<&str>,
        lines: Option<Vec<String>>,
    ) -> Vec<SubtitleCue> {
        let next = match (source, lines) {
            (Some(source), Some(lines)) if !lines.is_empty() => Some((source, lines)),
            _ => None,
        };
        let unchanged = match (&self.shown, &next) {
            (Some(shown), Some((source, lines))) => {
                shown.source == *source && shown.lines == *lines
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(2);
        if let Some(shown) = self.shown.take() {
            if now > shown.start {
                out.push(SubtitleCue {
                    slot: self.slot,
                    start: shown.start,
                    end: now,
                    lines: shown.lines,
                });
            }
        }
        if let Some((source, lines)) = next {
            out.push(SubtitleCue {
                slot: self.slot,
                start: now,
                end: SubtitleCue::OPEN_END,
                lines: lines.clone(),
            });
            self.shown = Some(Shown {
                source: source.to_owned(),
                lines,
                start: now,
            });
        }
        out
    }
}

/// One HLS output's subtitle slots: its route table, its cue feed and a
/// sampler per slot.
pub struct SubtitleOutputSampler {
    routes: SubtitleRoutes,
    feed: SubtitleFeed,
    slots: Vec<SubtitleSlotSampler>,
}

impl SubtitleOutputSampler {
    /// A sampler for `slot_count` slots routed by `routes`, feeding `feed`.
    #[must_use]
    pub fn new(routes: SubtitleRoutes, feed: SubtitleFeed, slot_count: usize) -> Self {
        let slots = (0..slot_count)
            .filter_map(|i| u8::try_from(i).ok())
            .map(SubtitleSlotSampler::new)
            .collect();
        Self {
            routes,
            feed,
            slots,
        }
    }

    /// Sample every slot at output instant `now` and offer each cue change
    /// to the feed (never blocks; a full feed drops and counts).
    pub fn tick(&mut self, now: MediaTime, resolve: &mut CueResolver) {
        let routes = self.routes.load();
        for (i, sampler) in self.slots.iter_mut().enumerate() {
            let source = routes.get(i).and_then(Option::as_deref);
            let lines = source.and_then(|id| resolve(id, now));
            for cue in sampler.sample(now, source, lines) {
                self.feed.offer(cue);
            }
        }
    }
}

/// Every HLS output's subtitle sampler for one run, plus the resolver. Built
/// once per run; ticked by the bake consumer.
pub struct SubtitleRun {
    outputs: Vec<SubtitleOutputSampler>,
    resolve: CueResolver,
    cadence: Rational,
    /// The last sampled tick, published for the patch API (F2: a re-point
    /// reports the tick it lands by). One relaxed-cost store per tick.
    last_tick: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl SubtitleRun {
    /// A run sampling `outputs` through `resolve` at `cadence`.
    #[must_use]
    pub fn new(
        outputs: Vec<SubtitleOutputSampler>,
        resolve: CueResolver,
        cadence: Rational,
    ) -> Self {
        Self {
            outputs,
            resolve,
            cadence,
            last_tick: None,
        }
    }

    /// Publish every sampled tick into `cell` (part F2).
    #[must_use]
    pub fn with_tick_cell(mut self, cell: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.last_tick = Some(cell);
        self
    }

    /// A run with no subtitle slot (one `is_empty` check per tick).
    #[must_use]
    pub fn none(cadence: Rational) -> Self {
        Self::new(Vec::new(), Box::new(|_, _| None), cadence)
    }

    /// Sample output tick `tick_index` (output time from the tick counter,
    /// never a float rate).
    pub fn tick(&mut self, tick_index: u64) {
        if self.outputs.is_empty() {
            return;
        }
        if let Some(cell) = &self.last_tick {
            cell.store(tick_index, std::sync::atomic::Ordering::Release);
        }
        let now = MediaTime::from_tick(i64::try_from(tick_index).unwrap_or(i64::MAX), self.cadence);
        for output in &mut self.outputs {
            output.tick(now, &mut self.resolve);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_output::hls::subtitle::subtitle_feed_with_capacity;

    #[allow(clippy::unnecessary_wraps)]
    // reason: the sampler and the resolver both take `Option<Vec<String>>`.
    fn lines(text: &str) -> Option<Vec<String>> {
        Some(vec![text.to_owned()])
    }

    fn at(tick: i64) -> MediaTime {
        MediaTime::from_tick(tick, Rational::new(25, 1))
    }

    #[test]
    fn a_cue_opens_on_appearance_and_closes_on_change() {
        let mut s = SubtitleSlotSampler::new(2);
        assert_eq!(s.sample(at(0), Some("a"), None), Vec::new());
        let opened = s.sample(at(1), Some("a"), lines("hello"));
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].start, at(1));
        assert!(opened[0].is_open());
        assert_eq!(opened[0].slot, 2);
        assert!(
            s.sample(at(2), Some("a"), lines("hello")).is_empty(),
            "unchanged"
        );
        let changed = s.sample(at(5), Some("a"), lines("world"));
        assert_eq!(changed.len(), 2);
        assert_eq!((changed[0].start, changed[0].end), (at(1), at(5)));
        assert_eq!(changed[0].lines, vec!["hello".to_owned()]);
        assert_eq!(changed[1].start, at(5));
        let cleared = s.sample(at(7), Some("a"), None);
        assert_eq!(cleared.len(), 1);
        assert_eq!((cleared[0].start, cleared[0].end), (at(5), at(7)));
        assert!(
            s.sample(at(8), Some("a"), Some(Vec::new())).is_empty(),
            "empty is none"
        );
    }

    #[test]
    fn a_repoint_lands_on_one_tick() {
        let (feed, rx) = subtitle_feed_with_capacity(16);
        let routes = subtitle_routes(vec![Some("cam_a".to_owned())]);
        let mut output = SubtitleOutputSampler::new(Arc::clone(&routes), feed, 1);
        let mut resolve: CueResolver = Box::new(|id, _| match id {
            "cam_a" => lines("from a"),
            "cam_b" => lines("from b"),
            _ => None,
        });
        output.tick(at(10), &mut resolve);
        output.tick(at(11), &mut resolve);
        // Re-point between ticks 11 and 12 (F2 swaps the table live).
        routes.store(Arc::from(vec![Some("cam_b".to_owned())]));
        output.tick(at(12), &mut resolve);
        let cues = rx.drain();
        assert_eq!(cues.len(), 3, "{cues:?}");
        assert_eq!(
            (cues[0].start, cues[0].lines[0].as_str()),
            (at(10), "from a")
        );
        assert!(cues[0].is_open());
        // Old cue ends at T, new one starts at T: the same tick.
        assert_eq!((cues[1].start, cues[1].end), (at(10), at(12)));
        assert_eq!(cues[1].lines[0], "from a");
        assert_eq!(
            (cues[2].start, cues[2].lines[0].as_str()),
            (at(12), "from b")
        );
        assert!(cues[2].is_open());
        // Un-patching closes at the next tick.
        routes.store(Arc::from(vec![None]));
        output.tick(at(13), &mut resolve);
        let cues = rx.drain();
        assert_eq!(cues.len(), 1);
        assert_eq!((cues[0].start, cues[0].end), (at(12), at(13)));
    }

    #[test]
    fn identical_text_on_a_new_source_is_a_new_cue() {
        let mut s = SubtitleSlotSampler::new(0);
        s.sample(at(0), Some("a"), lines("same"));
        let cues = s.sample(at(3), Some("b"), lines("same"));
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].end, at(3));
        assert_eq!(cues[1].start, at(3));
    }

    #[test]
    fn a_source_without_captions_yields_no_cues_and_run_uses_the_tick_clock() {
        let (feed, rx) = subtitle_feed_with_capacity(16);
        let routes = subtitle_routes(vec![Some("no_captions".to_owned()), None]);
        let output = SubtitleOutputSampler::new(routes, feed, 2);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let mut run = SubtitleRun::new(
            vec![output],
            Box::new(move |id, now| {
                log.lock().unwrap().push((id.to_owned(), now));
                None
            }),
            Rational::new(30_000, 1001),
        );
        run.tick(0);
        run.tick(30_000);
        assert_eq!(rx.drain(), Vec::new());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the unpatched slot asks nothing");
        assert_eq!(seen[1].1, MediaTime::from_nanos(1_001_000_000_000));
        SubtitleRun::none(Rational::new(25, 1)).tick(5);
    }
}
