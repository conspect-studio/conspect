//! On-air A/V offset measurement for live sources (audio locked to vision).
//!
//! Each live source has two [`SyncLog`]s: the video ingest records
//! `(stamp, raw_pts)` for every frame it publishes, and the audio placer records
//! every block it places on the output timeline. Once per output tick the bake
//! consumer (off the engine hot loop) asks, for each source: which raw PTS does
//! the tile latch at this tick's output instant (the tile's nearest-but-not-after
//! rule), and which raw PTS is the bus reading at this tick's sample position?
//! `video_raw - audio_raw` is the on-air lip-sync error in source time
//! (positive = audio leads the picture). It is logged as min/median/max every
//! 5 s and published per source as `av_offset_ms` (the latest window median)
//! for the status API.
//!
//! Everything here is lock-free on the per-tick path; the registry mutex is
//! taken only to look up a source's probe (plan build, and once per new source
//! id on the consumer).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use multiview_audio::sync_log::SyncLog;

/// Sentinel for "no offset measured" in [`AvProbe::offset_ms`].
const NO_OFFSET: i64 = i64::MIN;

/// One source's measurement state.
#[derive(Debug)]
pub struct AvProbe {
    /// Video frames published: `(stamp output ns, raw pts ns, 0)`.
    pub video: SyncLog,
    /// Audio placements on the output timeline (see [`SyncLog::placed`]).
    pub audio: Arc<SyncLog>,
    /// Latest window median offset (ms), or [`NO_OFFSET`].
    offset_ms: AtomicI64,
}

impl Default for AvProbe {
    fn default() -> Self {
        Self {
            video: SyncLog::default(),
            audio: Arc::new(SyncLog::default()),
            offset_ms: AtomicI64::new(NO_OFFSET),
        }
    }
}

impl AvProbe {
    /// The latest measured on-air offset in ms (video raw − audio raw; positive
    /// = audio ahead of the picture), or `None` before a measurement.
    #[must_use]
    pub fn offset_ms(&self) -> Option<i64> {
        let v = self.offset_ms.load(Ordering::Relaxed);
        (v != NO_OFFSET).then_some(v)
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<AvProbe>>> {
    static PROBES: OnceLock<Mutex<HashMap<String, Arc<AvProbe>>>> = OnceLock::new();
    PROBES.get_or_init(Default::default)
}

/// The process-wide probe for source `id` (the same `Arc` for every caller).
/// Takes a mutex: call once per plan/loop, never per frame.
pub fn probe(id: &str) -> Arc<AvProbe> {
    let mut map = registry().lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(map.entry(id.to_owned()).or_default())
}

/// The latest offset (ms) for each source that has one, for the status API.
#[must_use]
pub fn offsets() -> HashMap<String, i64> {
    let map = registry().lock().unwrap_or_else(PoisonError::into_inner);
    map.iter()
        .filter_map(|(id, p)| p.offset_ms().map(|o| (id.clone(), o)))
        .collect()
}

/// Video side: record a published live frame (non-blocking).
pub fn record_video(probe: &AvProbe, at: multiview_core::time::MediaTime, raw_ns: Option<i64>) {
    if let Some(raw) = raw_ns {
        probe.video.push(at.as_nanos(), raw, 0);
    }
}

/// The per-tick meter run on the bake consumer.
#[derive(Debug)]
pub struct AvMeter {
    cadence: multiview_core::time::Rational,
    rate: u32,
    window: HashMap<String, (Arc<AvProbe>, Vec<i64>)>,
    window_start: Option<std::time::Instant>,
}

/// How often the window is summarised and logged.
const WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

impl AvMeter {
    /// A meter for an output clock at `cadence` and a bus at `rate` Hz.
    #[must_use]
    pub fn new(cadence: multiview_core::time::Rational, rate: u32) -> Self {
        Self {
            cadence,
            rate,
            window: HashMap::new(),
            window_start: None,
        }
    }

    /// Sample every live source at output tick `tick_index`: the tile's latched
    /// video raw PTS at `pts_at(tick)` and the audio raw PTS at the bus's
    /// sample position `frame` for that tick (`SampleClock::total_at(tick)`).
    pub fn sample(&mut self, ids: &[String], tick_index: u64, frame: i64) {
        let now_ns = multiview_core::time::MediaTime::from_tick(
            i64::try_from(tick_index).unwrap_or(i64::MAX),
            self.cadence,
        )
        .as_nanos();
        for id in ids {
            let (p, values) = self
                .window
                .entry(id.clone())
                .or_insert_with(|| (probe(id), Vec::new()));
            let (Some(v), Some(a)) = (
                p.video.video_raw_at(now_ns),
                p.audio.audio_raw_at(frame, self.rate),
            ) else {
                continue;
            };
            if values.len() < 1_000 {
                values.push(v.saturating_sub(a) / 1_000_000);
            }
        }
        let start = *self
            .window_start
            .get_or_insert_with(std::time::Instant::now);
        if start.elapsed() >= WINDOW {
            self.window_start = Some(std::time::Instant::now());
            self.flush();
        }
    }

    fn flush(&mut self) {
        for (id, (p, values)) in &mut self.window {
            if values.is_empty() {
                p.offset_ms.store(NO_OFFSET, Ordering::Relaxed);
                tracing::info!(source = %id, "a/v offset: not measurable (no locked audio or no video)");
                continue;
            }
            values.sort_unstable();
            let min = values.first().copied().unwrap_or(0);
            let max = values.last().copied().unwrap_or(0);
            let median = values.get(values.len() / 2).copied().unwrap_or(0);
            p.offset_ms.store(median, Ordering::Relaxed);
            tracing::info!(
                source = %id,
                av_offset_ms_min = min,
                av_offset_ms_median = median,
                av_offset_ms_max = max,
                samples = values.len(),
                "a/v offset (video raw pts - audio raw pts at the same output instant; + = audio early)"
            );
            values.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Video and audio placed through the same mapping measure 0; audio
    /// placed 200 ms late (by raw time) measures +200.
    #[test]
    fn a_locked_source_measures_zero_and_a_slip_measures_the_slip() {
        let cadence = multiview_core::time::Rational::new(30, 1);
        for (slip_ms, want) in [(0i64, 0i64), (200, 200)] {
            let id = format!("av-test-{slip_ms}");
            let p = probe(&id);
            // Video: raw 100 s + k/30 at output 10 s + k/30.
            for k in 0..300i64 {
                let at = 10_000_000_000 + k * 1_000_000_000 / 30;
                record_video(
                    &p,
                    multiview_core::time::MediaTime::from_nanos(at),
                    Some(100_000_000_000 + k * 1_000_000_000 / 30),
                );
            }
            // Audio: raw 100 s at output 10 s (+ slip), 10 s of it.
            p.audio
                .placed(480_000, 480_000, 100_000_000_000 - slip_ms * 1_000_000);
            let mut m = AvMeter::new(cadence, 48_000);
            let ids = vec![id.clone()];
            for tick in 330..390u64 {
                let frame = i64::try_from(tick * 1_600).unwrap();
                m.sample(&ids, tick, frame);
            }
            m.flush();
            assert_eq!(p.offset_ms(), Some(want), "slip {slip_ms}");
        }
    }
}
