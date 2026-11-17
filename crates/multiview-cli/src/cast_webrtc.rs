//! The engine half of the cast `WebRTC` rung (ADR-0114 slice 5).
//!
//! Per cast output that has at least one `WebRTC` viewer, ONE feed:
//!
//! * one effective-output registry subscription (decision 6: one per cast
//!   output, however many viewers), drained by one off-clock thread;
//! * ONE Opus encoder (96 kbit/s), fed by decoding the output's own program
//!   AAC — the same audio the other rungs carry, so its content identity is
//!   the AAC slot's — through a bounded tap of [`OPUS_TAP_BLOCKS`] decoded
//!   blocks; the encoder is seeded from the engine timestamp of the first
//!   block it encodes, so its RTP time is engine time;
//! * one [`EgressSink`] → the driver's per-output [`EgressFeed`], bounded by
//!   count, bytes and age.
//!
//! Every window is judged by the identity gate of EACH viewer's attachment
//! (decision 5.4) before any of its samples is pushed; the driver writes a
//! sample to a viewer only when that viewer's attachment passed its window
//! ([`CastAttachment::may_write`]). Video and audio carry
//! `R = floor(E * clock / 1e6) mod 2^32` with `E` the engine media time
//! (decision 2, "WebRTC contract"); the RTCP wallclock is the epoch's wall
//! time of `E` for both streams. A viewer's PLI/FIR is counted and ignored.
//!
//! Isolation (invariants #1 and #10): nothing here runs on the clock or the
//! bake consumer. The feed thread is woken by the registry's bounded hand-off
//! and never blocks on a viewer; every queue drops.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use multiview_control::cast_streams::identity::{CastAttachment, Component, GateRefusal};
use multiview_control::cast_webrtc::{CastWebRtc, CastWhepAnswer, CastWhepReject};
use multiview_core::time::Rational;
use multiview_ffmpeg::{EncodedPacket, StreamKind};
use multiview_webrtc::egress::{egress_feed, CastStamp, EgressMedia, EgressSample, EgressSink};
use multiview_webrtc::transport::{ViewerGate, WhepServeHandle};

use crate::output_window_registry::{EffectiveBinding, Subscription, WindowSubscriptionSlot};

/// Decoded audio blocks the Opus tap holds (decision 6: 8 blocks).
pub const OPUS_TAP_BLOCKS: usize = 8;
/// Viewers per cast output (one token has at most two `WebRTC` attachments;
/// the run has at most 32 tokens).
pub const MAX_CAST_VIEWERS: u32 = 64;
/// How long the feed thread parks between empty polls.
const FEED_PARK: Duration = Duration::from_millis(5);

/// A cast attachment as the driver's viewer gate.
struct Gate(Arc<CastAttachment>);

impl ViewerGate for Gate {
    fn may_write(&self, window: u64) -> bool {
        self.0.may_write(window)
    }
    fn is_fenced(&self) -> bool {
        self.0.is_fenced()
    }
    fn fence(&self) {
        self.0.fence();
    }
}

/// One cast output's feed while it has viewers.
struct Feed {
    viewers: Arc<Mutex<Vec<Arc<CastAttachment>>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Counters a test or the status reads.
#[derive(Debug, Default)]
pub struct CastWebRtcStats {
    /// Opus encoders opened (one per cast output while it has viewers).
    pub opus_opened: AtomicU64,
    /// Opus encoders closed.
    pub opus_closed: AtomicU64,
    /// Windows pushed to a feed (passed by at least one viewer).
    pub windows: AtomicU64,
}

/// The cli's [`CastWebRtc`]: the shared WHEP-serve lane plus one feed per
/// cast output with viewers.
pub struct CliCastWebRtc {
    handle: WhepServeHandle,
    windows: WindowSubscriptionSlot,
    epochs: crate::cast_output::CastOutputsSlot,
    feeds: Mutex<HashMap<String, Feed>>,
    stats: Arc<CastWebRtcStats>,
}

impl CliCastWebRtc {
    /// Over the run's registry slot, its cast outputs and the WHEP lane.
    #[must_use]
    pub fn new(
        handle: WhepServeHandle,
        windows: WindowSubscriptionSlot,
        epochs: crate::cast_output::CastOutputsSlot,
    ) -> Self {
        Self {
            handle,
            windows,
            epochs,
            feeds: Mutex::new(HashMap::new()),
            stats: Arc::new(CastWebRtcStats::default()),
        }
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> Arc<CastWebRtcStats> {
        Arc::clone(&self.stats)
    }

    fn feeds(&self) -> std::sync::MutexGuard<'_, HashMap<String, Feed>> {
        self.feeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forget feeds whose viewers are all fenced (closes their Opus encoder
    /// and their registry subscription).
    fn prune(&self) {
        let mut gone = Vec::new();
        {
            let mut feeds = self.feeds();
            feeds.retain(|id, feed| {
                let live = feed
                    .viewers
                    .lock()
                    .map(|mut v| {
                        v.retain(|a| !a.is_fenced());
                        !v.is_empty()
                    })
                    .unwrap_or(false);
                if !live {
                    gone.push(id.clone());
                }
                live
            });
        }
        for id in gone {
            self.handle.unregister_output(&id);
        }
    }

    fn start_feed(&self, config_id: &str) -> Result<Feed, CastWhepReject> {
        let factory = self.windows.load().ok_or(CastWhepReject::Unavailable)?;
        let binding = factory
            .resolve(&multiview_control::WhepScope::Output(config_id.to_owned()))
            .map_err(|_| CastWhepReject::Unavailable)?;
        let epoch = self
            .epochs
            .load()
            .iter()
            .find(|e| e.id == config_id)
            .map(|e| e.epoch.clone())
            .ok_or(CastWhepReject::Unavailable)?;
        let subscription = factory
            .subscribe(
                &multiview_control::WhepScope::Output(config_id.to_owned()),
                binding.config_generation,
            )
            .map_err(|_| CastWhepReject::Unavailable)?;
        let (sink, feed) = egress_feed();
        self.handle
            .register_output(config_id, MAX_CAST_VIEWERS, feed);
        let viewers = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = FeedWorker::new(
            &binding,
            subscription,
            sink,
            Arc::clone(&viewers),
            epoch,
            Arc::clone(&self.stats),
        );
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("cast-webrtc-feed".to_owned())
            .spawn(move || worker.run(&thread_stop))
            .map_err(|_| {
                self.handle.unregister_output(config_id);
                CastWhepReject::Unavailable
            })?;
        Ok(Feed {
            viewers,
            stop,
            join: Some(join),
        })
    }
}

impl CastWebRtc for CliCastWebRtc {
    fn available(&self, config_id: &str) -> bool {
        self.windows.load().is_some_and(|f| {
            f.resolve(&multiview_control::WhepScope::Output(config_id.to_owned()))
                .is_ok()
        })
    }

    fn negotiate(
        &self,
        config_id: &str,
        offer: &str,
        attachment: Arc<CastAttachment>,
    ) -> Result<CastWhepAnswer, CastWhepReject> {
        self.prune();
        let viewers = {
            let mut feeds = self.feeds();
            if !feeds.contains_key(config_id) {
                let feed = self.start_feed(config_id)?;
                feeds.insert(config_id.to_owned(), feed);
            }
            feeds
                .get(config_id)
                .map(|f| Arc::clone(&f.viewers))
                .ok_or(CastWhepReject::Unavailable)?
        };
        let gate: Arc<dyn ViewerGate> = Arc::new(Gate(Arc::clone(&attachment)));
        let answer = match self.handle.negotiate_cast(config_id, offer, gate) {
            Ok(answer) => answer,
            Err(
                multiview_webrtc::WebRtcError::MalformedSdp(_)
                | multiview_webrtc::WebRtcError::NoCompatibleCodec,
            ) => {
                self.prune();
                return Err(CastWhepReject::Offer);
            }
            Err(_) => {
                self.prune();
                return Err(CastWhepReject::Unavailable);
            }
        };
        let session_id = answer.session_id.to_string();
        let handle = self.handle.clone();
        let (output, session) = (config_id.to_owned(), session_id.clone());
        attachment.set_closer(Box::new(move || {
            let _ = handle.release(&output, &session);
        }));
        viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(attachment);
        Ok(CastWhepAnswer {
            session_id,
            sdp: answer.answer_sdp,
        })
    }

    fn release(&self, config_id: &str, session_id: &str) -> bool {
        let released = self.handle.release(config_id, session_id);
        self.prune();
        released
    }
}

/// TEST SEAM: run one cast output's feed (subscription, gate, Opus) into
/// `sink` for `viewers`, on its own thread, without a WHEP transport.
#[cfg(test)]
pub(crate) fn spawn_feed_for_test(
    factory: &crate::output_window_registry::SubscriptionFactory,
    config_id: &str,
    sink: EgressSink,
    viewers: Arc<Mutex<Vec<Arc<CastAttachment>>>>,
    epoch: multiview_output::SharedEpoch,
    stats: Arc<CastWebRtcStats>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let scope = multiview_control::WhepScope::Output(config_id.to_owned());
    let binding = factory.resolve(&scope).unwrap();
    let subscription = factory
        .subscribe(&scope, binding.config_generation)
        .unwrap();
    let worker = FeedWorker::new(&binding, subscription, sink, viewers, epoch, stats);
    std::thread::spawn(move || worker.run(&stop))
}

/// The off-clock thread of one cast output's feed.
struct FeedWorker {
    subscription: Subscription,
    sink: EgressSink,
    viewers: Arc<Mutex<Vec<Arc<CastAttachment>>>>,
    epoch: multiview_output::SharedEpoch,
    stats: Arc<CastWebRtcStats>,
    video: Rational,
    cadence: Rational,
    /// The program AAC slot (its stream, time base, frame samples).
    audio: Option<(Rational, i64)>,
    decoder: Option<multiview_ffmpeg::decode_stream::StreamAudioDecoder>,
    opus: Option<multiview_ffmpeg::OpusEncoder>,
    /// Engine media time (µs) of the Opus encoder's sample 0.
    opus_base_us: Option<i64>,
    /// The decoded blocks waiting for the encoder (at most
    /// [`OPUS_TAP_BLOCKS`]; the oldest is dropped).
    tap: std::collections::VecDeque<(i64, Vec<f32>)>,
    window: u64,
    held: Option<crate::output_window_registry::SubscriptionWindow>,
}

impl FeedWorker {
    fn new(
        binding: &EffectiveBinding,
        subscription: Subscription,
        sink: EgressSink,
        viewers: Arc<Mutex<Vec<Arc<CastAttachment>>>>,
        epoch: multiview_output::SharedEpoch,
        stats: Arc<CastWebRtcStats>,
    ) -> Self {
        let audio = binding
            .audio_for(StreamKind::Audio)
            .map(|a| (a.time_base, 1024_i64));
        let decoder = binding.audio_for(StreamKind::Audio).and_then(|a| {
            let asc = a.config.as_ref()?;
            multiview_ffmpeg::decode_stream::StreamAudioDecoder::from_aac_config(
                asc,
                a.sample_rate,
                u16::try_from(a.channels).ok()?,
                a.time_base,
            )
            .ok()
        });
        Self {
            subscription,
            sink,
            viewers,
            epoch,
            stats,
            video: binding.video.time_base,
            cadence: binding.video.cadence,
            audio,
            decoder,
            opus: None,
            opus_base_us: None,
            tap: std::collections::VecDeque::with_capacity(OPUS_TAP_BLOCKS),
            window: 0,
            held: None,
        }
    }

    fn run(mut self, stop: &AtomicBool) {
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let next = match self.held.take() {
                Some(window) => Ok(Some(window)),
                None => self.subscription.try_recv_window(),
            };
            match next {
                Ok(Some(window)) => {
                    if !self.publish(&window) {
                        self.held = Some(window);
                        std::thread::park_timeout(FEED_PARK);
                    }
                }
                Ok(None) => std::thread::park_timeout(FEED_PARK),
                Err(_) => break,
            }
        }
        self.held = None;
        self.sink.close();
        if self.opus.take().is_some() {
            self.stats.opus_closed.fetch_add(1, Ordering::AcqRel);
        }
        for viewer in self
            .viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            viewer.fence();
        }
    }

    /// The `(component, tick)` of every packet of `packets` (an audio
    /// packet at its first and last sample); `None`: a packet has none.
    fn ticks(&self, packets: &[EncodedPacket]) -> Option<Vec<(Component, u64)>> {
        let mut out = Vec::with_capacity(packets.len() + 4);
        for packet in packets {
            let pts = packet.pts()?;
            match packet.kind() {
                StreamKind::Video => {
                    out.push((Component::Video, tick_of(pts, self.video, self.cadence)?))
                }
                StreamKind::Audio => {
                    let (base, span) = self.audio?;
                    out.push((Component::Audio, tick_of(pts, base, self.cadence)?));
                    out.push((
                        Component::Audio,
                        tick_of(pts.checked_add(span - 1)?, base, self.cadence)?,
                    ));
                }
                // The WebRTC rung carries the program-audio slot only.
                _ => {}
            }
        }
        Some(out)
    }

    /// Judge `window` for every viewer, then push its samples once.
    /// `false`: a viewer's gate holds it (not labelled yet): try again.
    fn publish(&mut self, window: &crate::output_window_registry::SubscriptionWindow) -> bool {
        let packets = window.permit.packets();
        let Some(ticks) = self.ticks(packets) else {
            for viewer in self.viewers() {
                viewer.fence();
            }
            return true;
        };
        let viewers = self.viewers();
        let mut any = false;
        let seq = self.window.saturating_add(1);
        for viewer in &viewers {
            if viewer.is_fenced() {
                continue;
            }
            match viewer.admit_window(ticks.iter().map(|(c, t)| (c, *t))) {
                Ok(()) => {
                    viewer.mark_passed(seq);
                    any = true;
                }
                Err(GateRefusal::NotYet) => return false,
                Err(GateRefusal::Fenced) => {}
            }
        }
        self.window = seq;
        if !any {
            return true;
        }
        self.stats.windows.fetch_add(1, Ordering::AcqRel);
        for packet in packets {
            match packet.kind() {
                StreamKind::Video => self.push_video(packet, seq),
                StreamKind::Audio => self.push_audio(packet, seq),
                _ => {}
            }
        }
        true
    }

    fn viewers(&self) -> Vec<Arc<CastAttachment>> {
        self.viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn stamp(&self, media_us: i64, seq: u64) -> CastStamp {
        CastStamp {
            window: seq,
            wallclock: wallclock_of(&self.epoch, media_us),
        }
    }

    fn push_video(&self, packet: &EncodedPacket, seq: u64) {
        let (Some(pts), Some(data)) = (packet.pts(), packet.payload()) else {
            return;
        };
        let media_us = micros_of(pts, self.video);
        let sample = EgressSample {
            media: EgressMedia::Video,
            rtp_timestamp: EgressMedia::Video.rtp_timestamp(pts, self.video),
            keyframe: packet.is_keyframe(),
            data: data.to_vec(),
            cast: Some(self.stamp(media_us, seq)),
        };
        let _ = self.sink.push(sample);
    }

    /// Decode the AAC packet into the tap (bounded), then encode what the
    /// one Opus encoder can take. Opus is stamped `E` of its first sample:
    /// the encoder is seeded with the engine time of the first block.
    fn push_audio(&mut self, packet: &EncodedPacket, seq: u64) {
        let Some((base, _)) = self.audio else {
            return;
        };
        let (Some(pts), Some(decoder)) = (packet.pts(), self.decoder.as_mut()) else {
            return;
        };
        let Ok(pcm) = decoder.decode_interleaved(packet) else {
            return;
        };
        if pcm.is_empty() {
            return;
        }
        if self.tap.len() >= OPUS_TAP_BLOCKS {
            self.tap.pop_front();
        }
        self.tap.push_back((micros_of(pts, base), pcm));
        if self.opus.is_none() {
            let Ok(encoder) =
                multiview_ffmpeg::OpusEncoder::new(multiview_ffmpeg::opus::PROGRAM_OPUS_BIT_RATE)
            else {
                return;
            };
            self.stats.opus_opened.fetch_add(1, Ordering::AcqRel);
            self.opus = Some(encoder);
            self.opus_base_us = self.tap.front().map(|(us, _)| *us);
        }
        let Some(base_us) = self.opus_base_us else {
            return;
        };
        let epoch = self.epoch.clone();
        while let Some((_, block)) = self.tap.pop_front() {
            let Some(opus) = self.opus.as_mut() else {
                return;
            };
            if opus.push_interleaved_f32(&block).is_err() {
                return;
            }
            while let Ok(Some(coded)) = opus.receive_packet() {
                // The encoder's own sample counter from `base_us`.
                let offset = coded.pts().unwrap_or(0);
                let media_us = base_us.saturating_add(offset.saturating_mul(1_000_000) / 48_000);
                let Some(data) = coded.payload() else {
                    continue;
                };
                let sample = EgressSample {
                    media: EgressMedia::Audio,
                    rtp_timestamp: rtp_of_micros(media_us, 48_000),
                    keyframe: false,
                    data: data.to_vec(),
                    cast: Some(CastStamp {
                        window: seq,
                        wallclock: wallclock_of(&epoch, media_us),
                    }),
                };
                let _ = self.sink.push(sample);
            }
        }
    }
}

/// The output tick a timestamp `pts` in `base` lies in.
fn tick_of(pts: i64, base: Rational, cadence: Rational) -> Option<u64> {
    let num = i128::from(pts)
        .checked_mul(i128::from(base.num))?
        .checked_mul(i128::from(cadence.num))?;
    let den = i128::from(base.den).checked_mul(i128::from(cadence.den))?;
    if den <= 0 {
        return None;
    }
    u64::try_from(num.div_euclid(den)).ok()
}

/// Engine media time `E` in µs of `pts` in `base` (exact integer floor).
fn micros_of(pts: i64, base: Rational) -> i64 {
    multiview_core::time::rescale(pts, base, Rational::new(1, 1_000_000))
}

/// `R = floor(E * clock / 1e6) mod 2^32` (decision 2).
fn rtp_of_micros(media_us: i64, clock: i64) -> u32 {
    let ticks = i128::from(media_us)
        .saturating_mul(i128::from(clock))
        .div_euclid(1_000_000);
    u32::try_from(ticks.rem_euclid(1_i128 << 32)).unwrap_or(0)
}

/// The epoch's wall time of `E`, mapped onto the monotonic clock (the RTCP
/// wallclock str0m takes is an `Instant`). `None` without a validated epoch
/// (review note R2-N3: the driver then uses the time of writing).
fn wallclock_of(epoch: &multiview_output::SharedEpoch, media_us: i64) -> Option<Instant> {
    let (map, _) = epoch.snapshot()?;
    if !map.rate.is_valid() {
        return None;
    }
    let wall_ns = map.wall_at(media_us.saturating_mul(1000));
    let now_ns = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    )
    .ok()?;
    let behind = now_ns.checked_sub(wall_ns)?;
    let behind = Duration::from_nanos(u64::try_from(behind).ok()?);
    Instant::now().checked_sub(behind)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;

    /// Decision 2: `R = floor(E * clock / 1e6) mod 2^32`, wrapping across
    /// 2^32 for both clocks, never clamped, no offset.
    #[test]
    fn rtp_time_is_engine_time_and_wraps() {
        assert_eq!(rtp_of_micros(1_000_000, 90_000), 90_000);
        assert_eq!(rtp_of_micros(1_000_000, 48_000), 48_000);
        // The 90 kHz wrap: 2^32 / 90 000 s = 47 721.858 844 s.
        let wrap_us = ((1_i64 << 32) * 1_000_000) / 90_000;
        let before = rtp_of_micros(wrap_us - 1_000, 90_000);
        let after = rtp_of_micros(wrap_us + 1_000, 90_000);
        assert!(after < before, "wrapped");
        assert_eq!(
            after.wrapping_sub(before),
            180,
            "2 ms of 90 kHz across the wrap"
        );
        // 48 kHz wrap.
        let wrap_us = ((1_i64 << 32) * 1_000_000) / 48_000;
        let before = rtp_of_micros(wrap_us - 20_000, 48_000);
        let after = rtp_of_micros(wrap_us + 20_000, 48_000);
        assert_eq!(after.wrapping_sub(before), 1920);
        // The video path's helper agrees with the formula for a tick PTS.
        let fps = Rational::new(1, 25);
        for tick in [0_i64, 1, 1_000_000, 1_193_046] {
            assert_eq!(
                EgressMedia::Video.rtp_timestamp(tick, fps),
                rtp_of_micros(micros_of(tick, fps), 90_000)
            );
        }
    }

    #[test]
    fn tick_of_maps_audio_and_video() {
        let cadence = Rational::new(25, 1);
        assert_eq!(tick_of(3, Rational::new(1, 25), cadence), Some(3));
        let aac = Rational::new(1, 48_000);
        assert_eq!(tick_of(1919, aac, cadence), Some(0));
        assert_eq!(tick_of(1920, aac, cadence), Some(1));
        // An AAC frame straddling ticks 2 and 3.
        assert_eq!(tick_of(5120, aac, cadence), Some(2));
        assert_eq!(tick_of(5120 + 1023, aac, cadence), Some(3));
    }
}
