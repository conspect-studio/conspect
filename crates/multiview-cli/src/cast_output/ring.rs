//! The pre-roll ring of a cast output: the most recent encoded packets, kept
//! in memory while no cast stream is active.
//!
//! Whole GOPs only: a GOP starts at an IDR and holds every video and audio
//! packet that arrived until the next IDR. The ring is bounded twice. By
//! time: the oldest GOP goes as soon as the GOPs after it still cover the
//! configured duration, so the ring holds at least that duration (once the
//! output has run that long) and at most one GOP more. By memory: over the
//! byte cap the oldest GOP goes, and a single GOP larger than the cap (an
//! encoder that stopped sending IDRs) empties the ring. Nothing here grows
//! without bound.

use std::collections::VecDeque;

use multiview_core::time::{rescale, Rational};

/// Bytes charged per packet on top of its payload (the record itself), so
/// a flood of empty packets is bounded too.
const PACKET_OVERHEAD: usize = 64;
/// How far before the next GOP's start an audio packet may begin and still
/// be carried over to that GOP when its own GOP is evicted (50 ms: more than
/// two AAC frames at 48 kHz).
const AUDIO_CARRY_NANOS: i64 = 50_000_000;
const NANOS: Rational = Rational::new(1, 1_000_000_000);

/// Which elementary stream a buffered packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingKind {
    /// An H.264 access unit.
    Video,
    /// A program-audio AAC frame.
    Audio,
}

/// One buffered packet: a copy of the payload with its timing and flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RingPacket {
    pub(crate) kind: RingKind,
    /// Presentation timestamp in the stream's own time base.
    pub(crate) pts: i64,
    /// Decode timestamp in the stream's own time base (audio: the pts).
    pub(crate) dts: i64,
    /// The access unit is an IDR (video only).
    pub(crate) keyframe: bool,
    /// The packet source reported a decoder-configuration change here.
    pub(crate) config_changed: bool,
    pub(crate) data: Vec<u8>,
}

impl RingPacket {
    fn cost(&self) -> usize {
        self.data.len().saturating_add(PACKET_OVERHEAD)
    }
}

#[derive(Debug)]
struct Gop {
    /// Presentation time of the GOP's IDR, in nanoseconds.
    start_ns: i64,
    /// Unix nanoseconds when the IDR arrived at the sink.
    arrived_unix_ns: i64,
    bytes: usize,
    packets: Vec<RingPacket>,
}

/// What [`PrerollRing::take`] hands to a new packager.
#[derive(Debug, Default)]
pub(crate) struct Preroll {
    /// `(pts in nanoseconds, Unix nanoseconds)` of the newest buffered IDR:
    /// the wall-clock anchor for the buffered packets.
    pub(crate) wall_anchor: Option<(i64, i64)>,
    /// The buffered packets in arrival order; the first one is an IDR.
    pub(crate) packets: Vec<RingPacket>,
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct PrerollRing {
    video_tb: Rational,
    audio_tb: Rational,
    max_nanos: i64,
    max_bytes: usize,
    gops: VecDeque<Gop>,
    bytes: usize,
    /// Decode time of the newest video packet, in nanoseconds.
    newest_ns: i64,
    last_video_dts: Option<i64>,
}

impl PrerollRing {
    /// A ring for video packets in `video_tb` and audio packets in
    /// `audio_tb` (seconds per timestamp unit), holding about `max_nanos` of
    /// media and at most `max_bytes`.
    pub(crate) fn new(
        video_tb: Rational,
        audio_tb: Rational,
        max_nanos: i64,
        max_bytes: usize,
    ) -> Self {
        Self {
            video_tb,
            audio_tb,
            max_nanos,
            max_bytes,
            gops: VecDeque::new(),
            bytes: 0,
            newest_ns: 0,
            last_video_dts: None,
        }
    }

    /// Forget everything (a packet gap: what is buffered no longer joins
    /// what comes next).
    pub(crate) fn clear(&mut self) {
        self.gops.clear();
        self.bytes = 0;
        self.newest_ns = 0;
        self.last_video_dts = None;
    }

    /// Buffer one video access unit. `keyframe` must be true only for an
    /// IDR. A non-IDR unit is dropped while the ring is empty, and a decode
    /// timestamp that does not advance empties the ring (a gap).
    pub(crate) fn push_video(&mut self, packet: RingPacket, arrived_unix_ns: i64) {
        if self.last_video_dts.is_some_and(|last| packet.dts <= last) {
            self.clear();
        }
        if !packet.keyframe && self.gops.is_empty() {
            return;
        }
        self.last_video_dts = Some(packet.dts);
        self.newest_ns = rescale(packet.dts, self.video_tb, NANOS);
        let cost = packet.cost();
        if packet.keyframe {
            self.gops.push_back(Gop {
                start_ns: rescale(packet.pts, self.video_tb, NANOS),
                arrived_unix_ns,
                bytes: cost,
                packets: vec![packet],
            });
        } else if let Some(gop) = self.gops.back_mut() {
            gop.bytes = gop.bytes.saturating_add(cost);
            gop.packets.push(packet);
        }
        self.bytes = self.bytes.saturating_add(cost);
        self.evict();
    }

    /// Buffer one audio frame with the GOP that is open (dropped while the
    /// ring is empty).
    pub(crate) fn push_audio(&mut self, packet: RingPacket) {
        let Some(gop) = self.gops.back_mut() else {
            return;
        };
        let cost = packet.cost();
        gop.bytes = gop.bytes.saturating_add(cost);
        gop.packets.push(packet);
        self.bytes = self.bytes.saturating_add(cost);
        self.evict();
    }

    /// Drop the oldest GOP. Its audio frames that start at (or just before)
    /// the next GOP move to the front of that GOP: audio that runs ahead of
    /// video must not leave the first buffered segment without its audio.
    fn pop_front(&mut self) {
        let Some(gone) = self.gops.pop_front() else {
            return;
        };
        self.bytes = self.bytes.saturating_sub(gone.bytes);
        let Some(next) = self.gops.front_mut() else {
            return;
        };
        let keep_from = next.start_ns.saturating_sub(AUDIO_CARRY_NANOS);
        let audio_tb = self.audio_tb;
        let mut carried: Vec<RingPacket> = gone
            .packets
            .into_iter()
            .filter(|packet| {
                packet.kind == RingKind::Audio && rescale(packet.pts, audio_tb, NANOS) >= keep_from
            })
            .collect();
        if carried.is_empty() {
            return;
        }
        let cost: usize = carried.iter().map(RingPacket::cost).sum();
        // The IDR stays first; the carried audio follows it.
        let rest = next.packets.split_off(1);
        next.packets.append(&mut carried);
        next.packets.extend(rest);
        next.bytes = next.bytes.saturating_add(cost);
        self.bytes = self.bytes.saturating_add(cost);
    }

    fn evict(&mut self) {
        // Time: the oldest GOP goes when the rest still covers the duration.
        while self
            .gops
            .get(1)
            .is_some_and(|second| self.newest_ns.saturating_sub(second.start_ns) >= self.max_nanos)
        {
            self.pop_front();
        }
        // Memory: the oldest GOP goes; a single oversized GOP empties the ring.
        while self.bytes > self.max_bytes {
            if self.gops.len() > 1 {
                self.pop_front();
            } else {
                self.clear();
            }
        }
    }

    /// Take everything out of the ring (it is empty afterwards).
    pub(crate) fn take(&mut self) -> Preroll {
        let wall_anchor = self
            .gops
            .back()
            .map(|gop| (gop.start_ns, gop.arrived_unix_ns));
        let mut packets = Vec::with_capacity(self.gops.iter().map(|gop| gop.packets.len()).sum());
        for gop in self.gops.drain(..) {
            packets.extend(gop.packets);
        }
        self.clear();
        Preroll {
            wall_anchor,
            packets,
        }
    }

    /// Bytes charged to the ring now.
    #[cfg(test)]
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Whole GOPs buffered now.
    #[cfg(test)]
    pub(crate) fn gops(&self) -> usize {
        self.gops.len()
    }

    /// Media time from the oldest buffered IDR to the newest video packet,
    /// in nanoseconds.
    #[cfg(test)]
    pub(crate) fn span_nanos(&self) -> i64 {
        self.gops
            .front()
            .map_or(0, |gop| self.newest_ns.saturating_sub(gop.start_ns))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{PrerollRing, RingKind, RingPacket, PACKET_OVERHEAD};
    use multiview_core::time::Rational;

    const VIDEO_TB: Rational = Rational::new(1, 25);
    const AUDIO_TB: Rational = Rational::new(1, 48_000);
    const SECOND: i64 = 1_000_000_000;

    fn video(index: i64, key: bool, size: usize) -> RingPacket {
        RingPacket {
            kind: RingKind::Video,
            pts: index,
            dts: index,
            keyframe: key,
            config_changed: false,
            data: vec![0xAB; size],
        }
    }

    fn audio(frame: i64) -> RingPacket {
        RingPacket {
            kind: RingKind::Audio,
            pts: frame * 1024,
            dts: frame * 1024,
            keyframe: false,
            config_changed: false,
            data: vec![0xCD; 100],
        }
    }

    /// Feed frames `range` at 25 fps with a 25-frame GOP and the audio
    /// frames that start before each video frame.
    fn feed(ring: &mut PrerollRing, range: std::ops::Range<i64>, next_audio: &mut i64) {
        for index in range {
            ring.push_video(video(index, index % 25 == 0, 1000), 1_000 + index);
            while *next_audio * 1024 * 25 < (index + 1) * 48_000 {
                ring.push_audio(audio(*next_audio));
                *next_audio += 1;
            }
        }
    }

    #[test]
    fn it_starts_at_an_idr_and_holds_whole_gops() {
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 20 * SECOND, 1 << 30);
        // Nothing is kept before the first IDR, audio included.
        ring.push_audio(audio(0));
        ring.push_video(video(3, false, 10), 0);
        assert_eq!((ring.gops(), ring.bytes()), (0, 0));
        let mut next_audio = 12;
        feed(&mut ring, 25..80, &mut next_audio);
        assert_eq!(ring.gops(), 3);
        let preroll = ring.take();
        let first = preroll.packets.first().unwrap();
        assert!(first.kind == RingKind::Video && first.keyframe && first.pts == 25);
        // The anchor is the newest IDR (frame 75 = 3 s) and its arrival time.
        assert_eq!(preroll.wall_anchor, Some((3 * SECOND, 1_075)));
        let videos: Vec<i64> = preroll
            .packets
            .iter()
            .filter(|p| p.kind == RingKind::Video)
            .map(|p| p.pts)
            .collect();
        assert_eq!(videos, (25..80).collect::<Vec<_>>());
        // The ring is empty after a take.
        assert_eq!((ring.gops(), ring.bytes(), ring.span_nanos()), (0, 0, 0));
        assert_eq!(ring.take().packets, Vec::new());
    }

    #[test]
    fn it_holds_between_20_and_21_seconds_whatever_the_run_length() {
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 20 * SECOND, 1 << 30);
        let mut next_audio = 0;
        let mut peak = 0;
        for chunk in 0..200 {
            feed(&mut ring, chunk * 25..(chunk + 1) * 25, &mut next_audio);
            peak = peak.max(ring.bytes());
            assert!(ring.span_nanos() <= 21 * SECOND, "{}", ring.span_nanos());
            assert!(ring.gops() <= 21, "{}", ring.gops());
            if chunk >= 21 {
                assert!(ring.span_nanos() >= 20 * SECOND, "{}", ring.span_nanos());
            }
        }
        // 200 s were fed; the ring never held more than 21 s of them.
        let per_second = 25 * (1000 + PACKET_OVERHEAD) + 48 * (100 + PACKET_OVERHEAD);
        assert!(peak <= 21 * per_second, "{peak}");
        let preroll = ring.take();
        let first = preroll.packets.first().unwrap();
        assert!(first.keyframe && first.pts % 25 == 0);
        assert!(first.pts >= 199 * 25 - 21 * 25, "{}", first.pts);
    }

    #[test]
    fn the_byte_cap_evicts_the_oldest_gops_and_an_oversized_gop_empties_the_ring() {
        let gop_bytes = 25 * (1000 + PACKET_OVERHEAD);
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 20 * SECOND, 3 * gop_bytes);
        for index in 0..250 {
            ring.push_video(video(index, index % 25 == 0, 1000), 0);
            assert!(ring.bytes() <= 3 * gop_bytes, "{}", ring.bytes());
        }
        assert!(ring.gops() <= 3);
        let first = ring.take().packets.first().unwrap().pts;
        assert_eq!(first % 25, 0, "still starts at an IDR");
        assert!(first >= 175, "{first}");

        // No IDR for a long time: the single open GOP outgrows the cap and
        // the ring empties; it refills only from the next IDR.
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 20 * SECOND, 10_000);
        ring.push_video(video(0, true, 1000), 0);
        for index in 1..200 {
            ring.push_video(video(index, false, 1000), 0);
            assert!(ring.bytes() <= 10_000, "{}", ring.bytes());
        }
        assert_eq!(ring.gops(), 0);
        ring.push_video(video(200, true, 1000), 0);
        assert_eq!(ring.gops(), 1);
        // A flood of empty packets is bounded by the per-packet charge.
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 20 * SECOND, 10_000);
        ring.push_video(video(0, true, 0), 0);
        for index in 1..10_000 {
            ring.push_video(video(index, false, 0), 0);
            assert!(ring.bytes() <= 10_000);
        }
    }

    #[test]
    fn clear_and_a_backwards_timestamp_drop_everything() {
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 20 * SECOND, 1 << 30);
        let mut next_audio = 0;
        feed(&mut ring, 0..60, &mut next_audio);
        assert_eq!(ring.gops(), 3);
        ring.clear();
        assert_eq!((ring.gops(), ring.bytes()), (0, 0));
        // After a gap the ring waits for an IDR again.
        ring.push_video(video(61, false, 10), 0);
        ring.push_audio(audio(999));
        assert_eq!(ring.bytes(), 0);
        feed(&mut ring, 75..110, &mut next_audio);
        assert_eq!(ring.gops(), 2);
        // A decode timestamp that does not advance is a gap too.
        ring.push_video(video(80, false, 10), 0);
        assert_eq!(ring.gops(), 0);
        ring.push_video(video(100, true, 10), 0);
        assert_eq!(ring.gops(), 1);
    }

    #[test]
    fn audio_that_runs_ahead_of_video_stays_with_the_gop_it_belongs_to() {
        // Audio arrives 2 frames (43 ms) before the video of the same time.
        let mut ring = PrerollRing::new(VIDEO_TB, AUDIO_TB, 2 * SECOND, 1 << 30);
        let mut next_audio = 0;
        for index in 0..200 {
            while next_audio * 1024 * 25 < (index + 1) * 48_000 + 2 * 1024 * 25 {
                ring.push_audio(audio(next_audio));
                next_audio += 1;
            }
            ring.push_video(video(index, index % 25 == 0, 1000), 0);
        }
        let preroll = ring.take();
        let first = preroll.packets.first().unwrap();
        assert!(first.kind == RingKind::Video && first.keyframe);
        let start_samples = first.pts * 48_000 / 25;
        let earliest_audio = preroll
            .packets
            .iter()
            .filter(|p| p.kind == RingKind::Audio)
            .map(|p| p.pts)
            .min()
            .unwrap();
        // An audio frame that starts within one frame of the first IDR is
        // buffered, so the first segment has audio from its start.
        assert!(
            earliest_audio <= start_samples && earliest_audio > start_samples - 3 * 1024,
            "{earliest_audio} vs {start_samples}"
        );
        // Audio pts stay in order after the carry.
        let audio_pts: Vec<i64> = preroll
            .packets
            .iter()
            .filter(|p| p.kind == RingKind::Audio)
            .map(|p| p.pts)
            .collect();
        assert!(audio_pts.windows(2).all(|w| w[0] < w[1]));
    }
}
