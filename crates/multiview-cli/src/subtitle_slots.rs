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
//! bounded [`SubtitleFeed`] (`try_send`, never blocks: invariant #10).
//!
//! **Bounded text before retention (ABU-1/INV-1).** The resolver projects the
//! active cue through [`bound_cue_lines`] while *borrowing* it from the cue
//! store's snapshot ([`text_cue_lines`]): bitmap cues are skipped without a
//! copy and text is cut to 4 lines / 512 bytes in fresh, exact-size
//! allocations, so the sampler only ever retains, compares, clones and
//! enqueues bounded text, whatever the source sends.
//!
//! **No lost close (MED-1/INV-2).** A cue change the full feed refuses is kept
//! in a small per-slot pending queue and retried (non-blocking `try_offer`) on
//! every later tick until accepted, so a close is never lost to a transient
//! overload, even when silence follows. The queue is bounded
//! ([`MAX_PENDING_PER_SLOT`]): an open cue superseded by its own close is
//! dropped (the close carries it), then the oldest closed cue whose open never
//! reached the sink (counted as a feed drop); the close of a cue the sink
//! holds open is never dropped.
//!
//! A re-point lands on one tick: the tick that first reads the new route
//! closes the old source's cue at that tick and may open the new source's
//! cue at the same tick.
//!
//! Pure Rust: the resolver (frame-store latch + caption cue store) is
//! supplied by the pipeline, so this module builds in every configuration.

use std::collections::VecDeque;
use std::sync::Arc;

use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::caption::CaptionCue;
use multiview_framestore::SwapCell;
use multiview_input::caption_store::CaptionCueStore;
use multiview_output::hls::{bound_cue_lines, BoundedLines, SubtitleCue, SubtitleFeed};

/// Most unsent cue changes a slot keeps for retry.
pub const MAX_PENDING_PER_SLOT: usize = 4;

/// One output's subtitle route table: slot index → patched source id
/// (`None` = unpatched). Replaced whole (RCU) by a re-patch.
pub type SubtitleRoutes = Arc<SwapCell<Arc<[Option<String>]>>>;

/// A route table initialised from the configured slot sources.
#[must_use]
pub fn subtitle_routes(sources: Vec<Option<String>>) -> SubtitleRoutes {
    Arc::new(SwapCell::new(Arc::from(sources)))
}

/// The resolver the pipeline supplies: the active caption lines of source
/// `id` at output instant `now` (the picture the compositor latches for that
/// tick), already bounded ([`bound_cue_lines`]), or `None` (no caption store,
/// no selected picture, no active text cue).
pub type CueResolver = Box<dyn FnMut(&str, MediaTime) -> Option<BoundedLines> + Send>;

/// The bounded text of the cue `store` holds for `generation` at `pts`,
/// projected while borrowing it from the store's snapshot: a bitmap cue (or
/// no cue) is `None` without copying its pixels, and only the bounded text
/// prefix of a text cue is copied (never the whole cue).
#[must_use]
pub fn text_cue_lines(
    store: &CaptionCueStore<CaptionCue>,
    pts: MediaTime,
    generation: u64,
) -> Option<BoundedLines> {
    store
        .with_active_at_generation(pts, generation, |cue| match cue {
            CaptionCue::Text { text, .. } => Some(bound_cue_lines(&text.lines)),
            _ => None,
        })
        .flatten()
        .filter(|lines| !lines.is_empty())
}

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
    /// current source (`None` = unpatched) and that source's active lines
    /// (bounded, so the state this keeps is bounded). Returns the cue changes
    /// in order: the closed previous cue (`start .. now`) when the text or the
    /// source changed, then the newly opened cue (`now ..`
    /// [`SubtitleCue::OPEN_END`]). Unchanged ⇒ nothing.
    pub fn sample(
        &mut self,
        now: MediaTime,
        source: Option<&str>,
        lines: Option<BoundedLines>,
    ) -> Vec<SubtitleCue> {
        let next = match (source, lines) {
            (Some(source), Some(lines)) if !lines.is_empty() => Some((source, lines.into_lines())),
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

/// One slot's unsent cue changes, retried on later ticks (MED-1/INV-2).
#[derive(Debug, Default)]
struct Pending {
    cues: VecDeque<SubtitleCue>,
    /// Start of the open cue the sink holds (delivered, its close not yet).
    delivered_open: Option<MediaTime>,
}

impl Pending {
    /// Queue `cue` behind the unsent ones, keeping at most
    /// [`MAX_PENDING_PER_SLOT`].
    fn push(&mut self, cue: SubtitleCue, feed: &SubtitleFeed) {
        if !cue.is_open() {
            // An unsent open cue superseded by its own close: the close
            // carries it.
            self.cues.retain(|c| !(c.is_open() && c.start == cue.start));
        }
        self.cues.push_back(cue);
        while self.cues.len() > MAX_PENDING_PER_SLOT {
            let protected = self.delivered_open;
            let victim = self
                .cues
                .iter()
                .position(|c| !c.is_open() && Some(c.start) != protected)
                .or_else(|| self.cues.iter().position(|c| Some(c.start) != protected));
            let Some(i) = victim else { break };
            self.cues.remove(i);
            feed.note_dropped();
        }
    }

    /// Offer the unsent cues in order until the feed refuses one (kept for
    /// the next tick). Never blocks.
    fn flush(&mut self, feed: &SubtitleFeed) {
        while let Some(cue) = self.cues.pop_front() {
            let (start, open) = (cue.start, cue.is_open());
            match feed.try_offer(cue) {
                Ok(()) => {
                    if open {
                        self.delivered_open = Some(start);
                    } else if self.delivered_open == Some(start) {
                        self.delivered_open = None;
                    }
                }
                Err(cue) => {
                    self.cues.push_front(cue);
                    break;
                }
            }
        }
    }
}

/// One HLS output's subtitle slots: its route table, its cue feed and a
/// sampler (plus its unsent changes) per slot.
pub struct SubtitleOutputSampler {
    routes: SubtitleRoutes,
    feed: SubtitleFeed,
    slots: Vec<(SubtitleSlotSampler, Pending)>,
}

impl SubtitleOutputSampler {
    /// A sampler for `slot_count` slots routed by `routes`, feeding `feed`.
    #[must_use]
    pub fn new(routes: SubtitleRoutes, feed: SubtitleFeed, slot_count: usize) -> Self {
        let slots = (0..slot_count)
            .filter_map(|i| u8::try_from(i).ok())
            .map(|slot| (SubtitleSlotSampler::new(slot), Pending::default()))
            .collect();
        Self {
            routes,
            feed,
            slots,
        }
    }

    /// Sample every slot at output instant `now` and offer its unsent cue
    /// changes, then this tick's, to the feed in order (never blocks; what a
    /// full feed refuses is retried on the next tick).
    pub fn tick(&mut self, now: MediaTime, resolve: &mut CueResolver) {
        let routes = self.routes.load();
        for (i, (sampler, pending)) in self.slots.iter_mut().enumerate() {
            pending.flush(&self.feed);
            let source = routes.get(i).and_then(Option::as_deref);
            let lines = source.and_then(|id| resolve(id, now));
            let truncated = lines.as_ref().is_some_and(BoundedLines::truncated);
            for cue in sampler.sample(now, source, lines) {
                if truncated && cue.is_open() {
                    self.feed.note_truncated();
                }
                pending.push(cue, &self.feed);
            }
            pending.flush(&self.feed);
        }
    }
}

/// Every HLS output's subtitle sampler for one run, plus the resolver. Built
/// once per run; ticked by the bake consumer.
pub struct SubtitleRun {
    outputs: Vec<SubtitleOutputSampler>,
    resolve: CueResolver,
    cadence: Rational,
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
        }
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
    // reason: the sampler and the resolver both take `Option<BoundedLines>`.
    fn lines(text: &str) -> Option<BoundedLines> {
        Some(bound_cue_lines(&[text]))
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
            s.sample(at(8), Some("a"), Some(bound_cue_lines::<&str>(&[])))
                .is_empty(),
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

    #[test]
    fn a_close_refused_by_a_full_feed_is_retried_until_the_held_cue_ends() {
        use multiview_output::hls::subtitle::SlotCues;
        use multiview_output::hls::SubtitleStats;
        let (feed, rx) = subtitle_feed_with_capacity(1);
        let routes = subtitle_routes(vec![Some("cam".to_owned())]);
        let mut output = SubtitleOutputSampler::new(routes, feed, 1);
        let showing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = Arc::clone(&showing);
        let mut resolve: CueResolver = Box::new(move |_, _| {
            flag.load(std::sync::atomic::Ordering::Relaxed)
                .then(|| bound_cue_lines(&["hello"]))
        });
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        // The open cue is delivered and reaches the sink.
        output.tick(at(1), &mut resolve);
        for cue in rx.drain() {
            held.push(cue, &stats);
        }
        assert!(held.overlapping(at(1), at(2))[0].is_open());
        // The sink stalls: something fills the one-cue feed.
        assert!(output.feed.offer(SubtitleCue {
            slot: 1,
            start: at(0),
            end: at(1),
            lines: vec!["filler".to_owned()],
        }));
        // The cue disappears while the feed is full: its close is refused.
        showing.store(false, std::sync::atomic::Ordering::Relaxed);
        output.tick(at(5), &mut resolve);
        // The sink recovers and drains (only the filler).
        assert_eq!(rx.drain().len(), 1);
        // Silence follows: the pending close is retried and delivered.
        output.tick(at(6), &mut resolve);
        output.tick(at(7), &mut resolve);
        for cue in rx.drain() {
            held.push(cue, &stats);
        }
        let cues = held.overlapping(at(0), at(1_000));
        assert_eq!(cues.len(), 1, "{cues:?}");
        assert_eq!(
            (cues[0].start, cues[0].end),
            (at(1), at(5)),
            "the held cue ends at its real end"
        );
        assert_eq!(
            held.overlapping(at(5), at(1_000)).len(),
            0,
            "nothing after the close"
        );
        assert_eq!(output.feed.stats().feed_dropped(), 0, "nothing was lost");
    }

    #[test]
    fn pending_changes_stay_bounded_and_keep_the_delivered_cues_close() {
        let (feed, rx) = subtitle_feed_with_capacity(1);
        let routes = subtitle_routes(vec![Some("cam".to_owned())]);
        let mut output = SubtitleOutputSampler::new(routes, feed, 1);
        let text = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let current = Arc::clone(&text);
        let mut resolve: CueResolver = Box::new(move |_, _| {
            let n = current.load(std::sync::atomic::Ordering::Relaxed);
            Some(bound_cue_lines(&[format!("caption {n}")]))
        });
        output.tick(at(0), &mut resolve);
        let first = rx.drain();
        assert_eq!(first.len(), 1);
        assert!(first[0].is_open());
        // Fill the feed, then change the text on 50 ticks while it stays full.
        assert!(output.feed.offer(first[0].clone()));
        for tick in 1..=50 {
            text.store(tick, std::sync::atomic::Ordering::Relaxed);
            output.tick(at(i64::try_from(tick).unwrap()), &mut resolve);
            assert!(output.slots[0].1.cues.len() <= MAX_PENDING_PER_SLOT);
        }
        assert!(
            output.feed.stats().feed_dropped() > 0,
            "overflow is counted"
        );
        // Recovery: the delivered cue's close (start at(0), end at(1)) is
        // still delivered, and the latest open cue reaches the sink.
        let mut seen = rx.drain();
        for tick in 51..60 {
            output.tick(at(tick), &mut resolve);
            seen.extend(rx.drain());
        }
        assert!(
            seen.iter().any(|c| c.start == at(0) && c.end == at(1)),
            "{seen:?}"
        );
        let last = seen.last().unwrap();
        assert!(last.is_open() && last.start == at(50), "{seen:?}");
    }

    #[test]
    fn oversized_text_is_bounded_in_sampler_state_and_queued_payloads() {
        use multiview_output::hls::subtitle::{MAX_CUE_LINES, MAX_CUE_TEXT_BYTES};
        let store = CaptionCueStore::<CaptionCue>::new();
        // ~256 KiB of text over 100 lines.
        let huge: Vec<String> = (0..100)
            .map(|i| format!("{i} {}", "x".repeat(2_600)))
            .collect();
        store.publish(
            at(0),
            at(100),
            CaptionCue::try_text(at(0), at(100), huge, None).unwrap(),
        );
        let projected = text_cue_lines(&store, at(1), store.generation()).unwrap();
        assert!(projected.truncated());
        let (feed, rx) = subtitle_feed_with_capacity(4);
        let routes = subtitle_routes(vec![Some("cam".to_owned())]);
        let mut output = SubtitleOutputSampler::new(routes, feed, 1);
        let shared = Arc::new(store);
        let reader = Arc::clone(&shared);
        let mut resolve: CueResolver =
            Box::new(move |_, now| text_cue_lines(&reader, now, reader.generation()));
        output.tick(at(1), &mut resolve);
        output.tick(at(2), &mut resolve);
        assert_eq!(output.feed.stats().truncated(), 1, "counted once per cue");
        let bounded = |lines: &Vec<String>| {
            lines.len() <= MAX_CUE_LINES
                && lines.capacity() <= MAX_CUE_LINES
                && lines.iter().map(|l| l.len() + 1).sum::<usize>() <= MAX_CUE_TEXT_BYTES + 1
                && lines.iter().all(|l| l.capacity() <= MAX_CUE_TEXT_BYTES)
        };
        let shown = output.slots[0].0.shown.as_ref().unwrap();
        assert!(bounded(&shown.lines), "sampler state");
        let queued = rx.drain();
        assert_eq!(queued.len(), 1);
        assert!(bounded(&queued[0].lines), "queued payload");
    }

    #[test]
    fn a_bitmap_cue_yields_no_text_cue() {
        let store = CaptionCueStore::<CaptionCue>::new();
        let rect = multiview_ffmpeg::caption::CueRect::new(0, 0, 64, 64);
        store.publish(
            at(0),
            at(100),
            CaptionCue::try_bitmap(at(0), at(100), vec![0; 64 * 64 * 4], rect).unwrap(),
        );
        assert_eq!(text_cue_lines(&store, at(1), store.generation()), None);
        assert_eq!(text_cue_lines(&store, at(1), store.generation() + 1), None);
        assert_eq!(text_cue_lines(&store, at(200), store.generation()), None);
    }

    /// Chaos (invariant #10: the engine never waits on a client; bounded
    /// queues drop, never grow): a wedged sink that NEVER drains the feed,
    /// 3 slots and a hostile resolver (256 KiB whitespace-padded text that
    /// changes every tick, alternating with nothing) for 10 000 ticks. Every
    /// tick completes, the feed never holds more than its capacity, pending
    /// stays bounded per slot, drops are counted, and the per-tick projection
    /// work stays within its budget. Once the sink recovers, silence still
    /// delivers the held cue's close (no open cue is left behind).
    #[test]
    #[allow(clippy::too_many_lines)]
    // reason: one chaos scenario (wedge, recovery) checked end to end.
    fn a_wedged_sink_never_blocks_or_grows_the_sampler() {
        use multiview_output::hls::subtitle::{SlotCues, MAX_INSPECTED_BYTES};
        use multiview_output::hls::SubtitleStats;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        const CAPACITY: usize = 8;
        const TICKS: u64 = 10_000;
        let (feed, rx) = subtitle_feed_with_capacity(CAPACITY);
        let stats = feed.stats();
        let routes = subtitle_routes(vec![
            Some("a".to_owned()),
            Some("b".to_owned()),
            Some("c".to_owned()),
        ]);
        let show = Arc::new(AtomicBool::new(false));
        let max_inspected = Arc::new(AtomicUsize::new(0));
        let pad = " ".repeat(256 * 1024);
        let resolve: CueResolver = {
            let (show, max_inspected) = (Arc::clone(&show), Arc::clone(&max_inspected));
            Box::new(move |id, now| {
                if !show.load(Ordering::Relaxed) {
                    return None;
                }
                // Changing text, then two 256 KiB whitespace-only lines and
                // more text: ~512 KiB of hostile source per sample.
                let head = format!("{id} {}", now.as_nanos());
                let source = [head.as_str(), pad.as_str(), pad.as_str(), "tail"];
                let bounded = bound_cue_lines(&source);
                max_inspected.fetch_max(bounded.inspected_bytes(), Ordering::Relaxed);
                Some(bounded)
            })
        };
        let run = SubtitleRun::new(
            vec![SubtitleOutputSampler::new(routes, feed, 3)],
            resolve,
            Rational::new(25, 1),
        );
        // Drive the wedged run on its own thread: a blocked tick would never
        // report back, so a bounded wait proves no tick blocks.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_show = Arc::clone(&show);
        std::thread::spawn(move || {
            let mut run = run;
            for tick in 0..TICKS {
                // Text on odd ticks (the last tick shows text), none on even.
                worker_show.store(tick % 2 == 1, Ordering::Relaxed);
                run.tick(tick);
                let pending_ok = run.outputs[0]
                    .slots
                    .iter()
                    .all(|(_, p)| p.cues.len() <= MAX_PENDING_PER_SLOT);
                if !pending_ok {
                    let _ = done_tx.send(Err(format!("pending grew at tick {tick}")));
                    return;
                }
            }
            let _ = done_tx.send(Ok(run));
        });
        let mut run = done_rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("every tick completes: the sampler never blocks on a wedged sink")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            max_inspected.load(Ordering::Relaxed) <= MAX_INSPECTED_BYTES,
            "per-tick projection work within budget"
        );
        assert!(stats.feed_dropped() > 0, "the wedge drops and counts");
        // The sink recovers: the feed never held more than its capacity.
        let sink_stats = SubtitleStats::default();
        let mut held: Vec<SlotCues> = (0..3).map(|_| SlotCues::default()).collect();
        let mut deliver = |cues: Vec<SubtitleCue>| {
            for cue in cues {
                held[usize::from(cue.slot)].push(cue, &sink_stats);
            }
        };
        let wedged = rx.drain();
        assert!(wedged.len() <= CAPACITY, "{}", wedged.len());
        assert!(wedged.iter().all(|c| c.lines.len() <= 4));
        deliver(wedged);
        // Silence follows: the pending changes and the held cues' closes
        // arrive within a few ticks.
        show.store(false, Ordering::Relaxed);
        for tick in TICKS..TICKS + 8 {
            run.tick(tick);
            deliver(rx.drain());
        }
        assert!(run.outputs[0].slots.iter().all(|(_, p)| p.cues.is_empty()));
        for (slot, cues) in held.iter().enumerate() {
            let open = cues
                .overlapping(MediaTime::ZERO, SubtitleCue::OPEN_END)
                .into_iter()
                .filter(|c| c.is_open())
                .count();
            assert_eq!(open, 0, "slot {slot} keeps no OPEN_END cue");
        }
    }
}
