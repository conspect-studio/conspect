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
/// Opus look-ahead in samples at 48 kHz (libopus: 312; the native encoder's
/// is smaller): input this far before a packet's first sample may be coded
/// into it.
const OPUS_LOOKAHEAD: i64 = 960;
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

/// A feed's viewers and whether the feed is closed. One lock decides both:
/// the worker closes the feed only when no viewer is live and no admission is
/// pending, and an admission is appended only to a feed that is not closed
/// (PR 521 review round 2), so no viewer is ever admitted to a dead feed nor
/// fenced by a retirement decided before it arrived.
#[derive(Default)]
pub(crate) struct Viewers {
    list: Vec<Arc<CastAttachment>>,
    closed: bool,
}

impl Viewers {
    /// An open feed's viewers (the pipeline tests).
    #[cfg(all(test, feature = "gpl-codecs"))]
    pub(crate) fn of(list: Vec<Arc<CastAttachment>>) -> Self {
        Self {
            list,
            closed: false,
        }
    }
}

/// One cast output's feed while it has viewers. The worker thread owns its
/// retirement (PR 521 review): it ends itself once no viewer is live and no
/// admission is in flight, and marks `done`; the control plane only forgets
/// finished feeds and never joins under a lock.
struct Feed {
    viewers: Arc<Mutex<Viewers>>,
    /// Admissions in flight (an offer being negotiated): the worker does not
    /// retire while one is pending.
    pending: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        // Detached: the worker sees `stop` within one park and exits.
        self.stop.store(true, Ordering::Release);
    }
}

/// How long a feed with no live viewer lingers before it retires (a
/// receiver re-attaching after a step-down keeps its encoder).
const FEED_LINGER: Duration = Duration::from_secs(2);

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

    /// Forget feeds whose worker retired (it unregistered itself).
    fn prune(&self) {
        self.feeds()
            .retain(|_, feed| !feed.done.load(Ordering::Acquire));
    }

    fn start_feed(&self, config_id: &str) -> Result<Feed, CastWhepReject> {
        let factory = self.windows.load().ok_or(CastWhepReject::Unavailable)?;
        let scope = multiview_control::WhepScope::Output(config_id.to_owned());
        let binding = factory
            .resolve(&scope)
            .map_err(|_| CastWhepReject::Unavailable)?;
        let epoch = self
            .epochs
            .load()
            .iter()
            .find(|e| e.id == config_id)
            .map(|e| e.epoch.clone())
            .ok_or(CastWhepReject::Unavailable)?;
        let subscription = factory
            .subscribe(&scope, binding.config_generation)
            .map_err(|_| CastWhepReject::Unavailable)?;
        let (sink, feed) = egress_feed();
        let generation = self
            .handle
            .register_output_generation(config_id, MAX_CAST_VIEWERS, feed);
        let viewers = Arc::new(Mutex::new(Viewers::default()));
        // The admission that starts the feed is pending from the start.
        let pending = Arc::new(AtomicU64::new(1));
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let worker = FeedWorker::new(
            &binding,
            subscription,
            sink,
            Arc::clone(&viewers),
            epoch,
            Arc::clone(&self.stats),
        );
        let retire = Retire {
            handle: Some(self.handle.clone()),
            output: config_id.to_owned(),
            generation,
            pending: Arc::clone(&pending),
            done: Arc::clone(&done),
        };
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("cast-webrtc-feed".to_owned())
            .spawn(move || {
                worker.run(&thread_stop, &retire);
                retire.finish();
            })
            .map_err(|_| {
                self.handle
                    .unregister_output_generation(config_id, generation);
                CastWhepReject::Unavailable
            })?;
        Ok(Feed {
            viewers,
            pending,
            stop,
            done,
        })
    }
}

/// `pending` of a retired feed: no admission may reserve it again.
const CLOSED: u64 = u64::MAX;

/// Reserve one admission on a feed unless it is closed.
fn reserve(pending: &AtomicU64) -> bool {
    let mut n = pending.load(Ordering::Acquire);
    loop {
        if n == CLOSED {
            return false;
        }
        match pending.compare_exchange_weak(
            n,
            n.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(seen) => n = seen,
        }
    }
}

/// What a feed worker needs to retire itself.
struct Retire {
    /// `None` in the test seam (no WHEP lane).
    handle: Option<WhepServeHandle>,
    output: String,
    generation: u64,
    pending: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
}

impl Retire {
    /// No admission may reserve this feed any more.
    fn close(&self) {
        self.pending.store(CLOSED, Ordering::Release);
    }

    fn finish(&self) {
        // Only this feed's registration: a newer feed of the same output
        // registered meanwhile keeps its own.
        if let Some(handle) = &self.handle {
            handle.unregister_output_generation(&self.output, self.generation);
        }
        self.done.store(true, Ordering::Release);
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
        // Reserve the admission under the lock (the worker never retires
        // while one is pending), then negotiate outside it.
        let (viewers, pending) = {
            let mut feeds = self.feeds();
            let live = feeds
                .get(config_id)
                .filter(|f| !f.done.load(Ordering::Acquire) && !f.stop.load(Ordering::Acquire));
            if live.is_some_and(|feed| reserve(&feed.pending)) {
            } else {
                let feed = self.start_feed(config_id)?;
                feeds.insert(config_id.to_owned(), feed);
            }
            let feed = feeds.get(config_id).ok_or(CastWhepReject::Unavailable)?;
            (Arc::clone(&feed.viewers), Arc::clone(&feed.pending))
        };
        let gate: Arc<dyn ViewerGate> = Arc::new(Gate(Arc::clone(&attachment)));
        let result = self.handle.negotiate_cast(config_id, offer, gate);
        let outcome = match result {
            Ok(answer) => {
                let session_id = answer.session_id.to_string();
                let handle = self.handle.clone();
                let (output, session) = (config_id.to_owned(), session_id.clone());
                attachment.set_closer(Box::new(move || {
                    let _ = handle.release(&output, &session);
                }));
                let admitted = {
                    let mut viewers = viewers.lock().unwrap_or_else(PoisonError::into_inner);
                    if viewers.closed {
                        false
                    } else {
                        viewers.list.push(Arc::clone(&attachment));
                        true
                    }
                };
                if admitted {
                    Ok(CastWhepAnswer {
                        session_id,
                        sdp: answer.answer_sdp,
                    })
                } else {
                    // The feed ended while this offer negotiated: its session
                    // is released at once (the closer) and the receiver steps
                    // down.
                    attachment.fence();
                    Err(CastWhepReject::Unavailable)
                }
            }
            Err(
                multiview_webrtc::WebRtcError::MalformedSdp(_)
                | multiview_webrtc::WebRtcError::NoCompatibleCodec,
            ) => Err(CastWhepReject::Offer),
            Err(_) => Err(CastWhepReject::Unavailable),
        };
        // The viewer (if any) is in the list before the reservation ends.
        pending.fetch_sub(1, Ordering::AcqRel);
        outcome
    }

    fn release(&self, config_id: &str, session_id: &str) -> bool {
        self.handle.release(config_id, session_id)
    }
}

/// TEST SEAM: run one cast output's feed (subscription, gate, Opus) into
/// `sink` for `viewers`, on its own thread, without a WHEP transport.
#[cfg(all(test, feature = "gpl-codecs"))]
#[allow(clippy::unwrap_used)] // A test seam: a refused resolve is the test's failure.
pub(crate) fn spawn_feed_for_test(
    factory: &crate::output_window_registry::SubscriptionFactory,
    config_id: &str,
    sink: EgressSink,
    viewers: Arc<Mutex<Viewers>>,
    epoch: multiview_output::SharedEpoch,
    stats: Arc<CastWebRtcStats>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let scope = multiview_control::WhepScope::Output(config_id.to_owned());
    let binding = factory.resolve(&scope).unwrap();
    let subscription = factory
        .subscribe(&scope, binding.config_generation)
        .unwrap();
    let worker = FeedWorker::new(&binding, subscription, sink, viewers, epoch, stats);
    let retire = Retire {
        handle: None,
        output: config_id.to_owned(),
        generation: 0,
        pending: Arc::new(AtomicU64::new(0)),
        done: Arc::new(AtomicBool::new(false)),
    };
    std::thread::spawn(move || {
        worker.run(&stop, &retire);
        retire.finish();
    })
}

/// The off-clock thread of one cast output's feed.
struct FeedWorker {
    subscription: Subscription,
    sink: EgressSink,
    viewers: Arc<Mutex<Viewers>>,
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
    /// The decoded blocks waiting for the encoder, with their engine time
    /// and window (at most [`OPUS_TAP_BLOCKS`]; the oldest is dropped).
    tap: std::collections::VecDeque<(i64, u64, Vec<f32>)>,
    /// Samples given to the Opus encoder since it was seeded.
    fed: i64,
    /// `(first encoder sample, window)` of every block given to the encoder
    /// that a coming packet can still contain (bounded: pruned as packets
    /// are emitted; at most a few blocks of staging and look-ahead).
    provenance: std::collections::VecDeque<(i64, u64)>,
    window: u64,
    held: Option<crate::output_window_registry::SubscriptionWindow>,
}

impl FeedWorker {
    fn new(
        binding: &EffectiveBinding,
        subscription: Subscription,
        sink: EgressSink,
        viewers: Arc<Mutex<Viewers>>,
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
            fed: 0,
            provenance: std::collections::VecDeque::with_capacity(OPUS_TAP_BLOCKS),
            window: 0,
            held: None,
        }
    }

    fn run(mut self, stop: &AtomicBool, retire: &Retire) {
        let mut idle_since: Option<Instant> = None;
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            // Retire once no viewer is live and no admission is pending, for
            // FEED_LINGER (the last viewer's DELETE, revoke or death).
            let idle = {
                let mut viewers = self.viewers.lock().unwrap_or_else(PoisonError::into_inner);
                viewers.list.retain(|v| !v.is_fenced());
                viewers.list.is_empty()
            };
            if !idle || retire.pending.load(Ordering::Acquire) > 0 {
                idle_since = None;
            } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= FEED_LINGER {
                // Decide under the viewers lock: still no live viewer, and no
                // admission reserved (CAS to CLOSED: `reserve` refuses it).
                let mut viewers = self.viewers.lock().unwrap_or_else(PoisonError::into_inner);
                viewers.list.retain(|v| !v.is_fenced());
                if viewers.list.is_empty()
                    && retire
                        .pending
                        .compare_exchange(0, CLOSED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    viewers.closed = true;
                    break;
                }
                idle_since = None;
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
        // Every exit (retired, stopped, producer ended) closes admission
        // first: an offer negotiating now finds the feed closed and fences its
        // own attachment. Then every viewer still here is fenced.
        retire.close();
        let fenced = {
            let mut viewers = self.viewers.lock().unwrap_or_else(PoisonError::into_inner);
            viewers.closed = true;
            std::mem::take(&mut viewers.list)
        };
        for viewer in fenced {
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
                    out.push((Component::Video, tick_of(pts, self.video, self.cadence)?));
                }
                StreamKind::Audio => {
                    let (base, span) = self.audio?;
                    // Every tick the frame covers (three at 60 fps).
                    let first = tick_of(pts, base, self.cadence)?;
                    let last = tick_of(pts.checked_add(span - 1)?, base, self.cadence)?;
                    if last < first || last - first > 64 {
                        return None;
                    }
                    out.extend((first..=last).map(|t| (Component::Audio, t)));
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
            .list
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
            // Decision 2: `R = floor(E * 90000 / 1e6) mod 2^32`, from `E`
            // itself (the receiver maps `R` back with the same formula).
            rtp_timestamp: rtp_of_micros(media_us, 90_000),
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
            // A decode error is a gap: the next block re-seeds.
            self.reset_opus();
            return;
        };
        if pcm.is_empty() {
            return;
        }
        if self.tap.len() >= OPUS_TAP_BLOCKS {
            // A dropped block is a gap too (never concatenated across).
            self.tap.pop_front();
            self.reset_opus();
        }
        self.tap.push_back((micros_of(pts, base), seq, pcm));
        self.drain_tap();
    }

    /// Drop the Opus encoder and its staging (a gap in the audio: the
    /// next block re-seeds it at that block's own engine time, so the Opus
    /// timeline never compresses a gap; PR 521 review).
    fn reset_opus(&mut self) {
        if self.opus.take().is_some() {
            self.stats.opus_closed.fetch_add(1, Ordering::AcqRel);
        }
        self.opus_base_us = None;
        self.fed = 0;
        self.provenance.clear();
    }

    /// Encode what the tap holds. Each block's engine time is checked
    /// against the encoder's running sample position; a mismatch beyond
    /// one sample is a gap and re-seeds the encoder. Every Opus packet is
    /// stamped with the OLDEST window any of its input samples (staging and
    /// codec delay included) came from, so a viewer that joined later is
    /// never written audio of a window it did not judge.
    fn drain_tap(&mut self) {
        let epoch = self.epoch.clone();
        while let Some((block_us, seq, block)) = self.tap.pop_front() {
            if let Some(base_us) = self.opus_base_us {
                let expected = base_us.saturating_add(self.fed.saturating_mul(1_000_000) / 48_000);
                if (block_us - expected).abs() > 1_000_000 / 48_000 + 1 {
                    self.reset_opus();
                }
            }
            if self.opus.is_none() {
                let Ok(encoder) = multiview_ffmpeg::OpusEncoder::new(
                    multiview_ffmpeg::opus::PROGRAM_OPUS_BIT_RATE,
                ) else {
                    return;
                };
                self.stats.opus_opened.fetch_add(1, Ordering::AcqRel);
                self.opus = Some(encoder);
                self.opus_base_us = Some(block_us);
            }
            let Some(base_us) = self.opus_base_us else {
                return;
            };
            let frames = i64::try_from(block.len() / 2).unwrap_or(0);
            self.provenance.push_back((self.fed, seq));
            self.fed = self.fed.saturating_add(frames);
            let Some(opus) = self.opus.as_mut() else {
                return;
            };
            if opus.push_interleaved_f32(&block).is_err() {
                self.reset_opus();
                return;
            }
            let mut coded_out = Vec::new();
            while let Ok(Some(coded)) = opus.receive_packet() {
                coded_out.push(coded);
            }
            for coded in coded_out {
                // The encoder's own sample counter from `base_us`.
                let offset = coded.pts().unwrap_or(0);
                let media_us = base_us.saturating_add(offset.saturating_mul(1_000_000) / 48_000);
                // Input that can be inside this packet: from its first sample
                // less the codec look-ahead, to its end.
                let first = offset.saturating_sub(OPUS_LOOKAHEAD);
                let window = self.oldest_window_from(first);
                // Forget provenance no later packet can reach.
                while self.provenance.len() > 1
                    && self
                        .provenance
                        .get(1)
                        .is_some_and(|(start, _)| *start <= first)
                {
                    self.provenance.pop_front();
                }
                let Some(data) = coded.payload() else {
                    continue;
                };
                let sample = EgressSample {
                    media: EgressMedia::Audio,
                    rtp_timestamp: rtp_of_micros(media_us, 48_000),
                    keyframe: false,
                    data: data.to_vec(),
                    cast: Some(CastStamp {
                        window,
                        wallclock: wallclock_of(&epoch, media_us),
                    }),
                };
                let _ = self.sink.push(sample);
            }
        }
    }

    /// The oldest window whose samples start at or after `first` can
    /// overlap — the window of the block containing sample `first`.
    fn oldest_window_from(&self, first: i64) -> u64 {
        self.provenance
            .iter()
            .rev()
            .find(|(start, _)| *start <= first)
            .or_else(|| self.provenance.front())
            .map_or(0, |(_, seq)| *seq)
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

/// Engine media time `E` in µs of `pts` in `base` (exact integer floor;
/// decision 2: `E` is the PTS in µs, truncated).
fn micros_of(pts: i64, base: Rational) -> i64 {
    let num = i128::from(pts)
        .saturating_mul(i128::from(base.num))
        .saturating_mul(1_000_000);
    let den = i128::from(base.den.max(1));
    i64::try_from(num.div_euclid(den)).unwrap_or(i64::MAX)
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
        // Fractional cadences (PR 521 review): E is floored to µs first,
        // then R is floored: 30 fps tick 1 is E = 33 333 µs, R = 2 999;
        // 30000/1001 tick 1 is E = 33 366 µs, R = 3 002.
        assert_eq!(micros_of(1, Rational::new(1, 30)), 33_333);
        assert_eq!(
            rtp_of_micros(micros_of(1, Rational::new(1, 30)), 90_000),
            2_999
        );
        assert_eq!(micros_of(1, Rational::new(1001, 30_000)), 33_366);
        assert_eq!(
            rtp_of_micros(micros_of(1, Rational::new(1001, 30_000)), 90_000),
            3_002
        );
        // 25 fps is exact: tick t is t * 3600.
        assert_eq!(
            rtp_of_micros(micros_of(7, Rational::new(1, 25)), 90_000),
            25_200
        );
    }

    // ---- the feed lifecycle on a real WHEP-serve lane and registry ----

    use multiview_control::cast_streams::identity::{Clearance, OutputLabels};
    use multiview_webrtc::transport::{Direction, MediaKind, Session, SessionConfig};

    fn offer() -> String {
        let mut viewer = Session::new(&SessionConfig::default(), Instant::now());
        viewer
            .add_host_candidate("[::1]:50000".parse().unwrap())
            .unwrap();
        viewer
            .create_offer_with_direction(&[MediaKind::Video, MediaKind::Audio], Direction::RecvOnly)
            .unwrap()
    }

    struct Rig {
        rung: Arc<CliCastWebRtc>,
        handle: WhepServeHandle,
        factory: Arc<crate::output_window_registry::SubscriptionFactory>,
        registry: crate::output_window_registry::OutputWindowRegistry,
        _endpoint: multiview_webrtc::transport::WhepServeEndpoint,
    }

    fn rig() -> Rig {
        let config = multiview_webrtc::config::EndpointConfig {
            udp_port: 0,
            advertised_addresses: vec!["2001:db8::1".parse().unwrap()],
            ..multiview_webrtc::config::EndpointConfig::default()
        };
        let (endpoint, handle) =
            multiview_webrtc::transport::WhepServeEndpoint::bind(config).unwrap();
        let binding = crate::output_window_registry::tests::binding("cast-1", 0);
        let generation = binding.config_generation;
        let (registry, factory) =
            crate::output_window_registry::OutputWindowRegistry::new(vec![binding], generation)
                .unwrap();
        let factory = Arc::new(factory);
        let windows = crate::output_window_registry::window_subscription_slot();
        windows.store(Some(Arc::clone(&factory)));
        let outputs = crate::cast_output::cast_outputs_slot();
        outputs.store(Arc::new(vec![crate::cast_output::CastOutputEntry {
            id: "cast-1".into(),
            dir: std::path::PathBuf::new(),
            gate: Arc::new(crate::cast_output::CastGate::new()),
            width: 1920,
            height: 1080,
            fps: Rational::new(25, 1),
            codec: String::new(),
            epoch: multiview_output::SharedEpoch::default(),
            labels: Arc::new(OutputLabels::new()),
            index: 0,
            tracks: Vec::new(),
        }]));
        Rig {
            rung: Arc::new(CliCastWebRtc::new(handle.clone(), windows, outputs)),
            handle,
            factory,
            registry,
            _endpoint: endpoint,
        }
    }

    fn attachment() -> Arc<CastAttachment> {
        Arc::new(CastAttachment::new(
            "s",
            "cast-1",
            "webrtc",
            Arc::new(OutputLabels::new()),
            Clearance::default(),
        ))
    }

    fn wait(what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// PR 521 review: the last viewer leaving (DELETE through its fence)
    /// retires the feed by itself: the registry subscription and the WHEP
    /// registration are released without another request.
    #[test]
    fn the_last_viewer_leaving_retires_the_feed_without_another_request() {
        let r = rig();
        let a = attachment();
        r.rung
            .negotiate("cast-1", &offer(), Arc::clone(&a))
            .unwrap();
        assert_eq!(
            r.factory.slots_in_use(),
            1,
            "one subscription per cast output"
        );
        let b = attachment();
        r.rung
            .negotiate("cast-1", &offer(), Arc::clone(&b))
            .unwrap();
        assert_eq!(r.factory.slots_in_use(), 1, "a second viewer shares it");
        a.fence();
        b.fence();
        wait("the feed retired", || r.factory.slots_in_use() == 0);
        wait("the registration removed", || {
            r.handle.negotiate("cast-1", &offer(), true).is_err()
        });
    }

    /// PR 521 review: concurrent first offers never orphan a viewer: every
    /// admitted viewer's feed is the live one (each of them is served), and
    /// one subscription serves them all.
    #[test]
    fn concurrent_first_offers_share_one_live_feed() {
        let r = rig();
        let viewers: Vec<_> = (0..8).map(|_| attachment()).collect();
        let threads: Vec<_> = viewers
            .iter()
            .map(|a| {
                let (rung, a) = (Arc::clone(&r.rung), Arc::clone(a));
                std::thread::spawn(move || rung.negotiate("cast-1", &offer(), a).is_ok())
            })
            .collect();
        let admitted = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(admitted, 8);
        assert_eq!(r.factory.slots_in_use(), 1);
        let feeds = r.rung.feeds();
        let feed = feeds.get("cast-1").expect("one live feed");
        assert_eq!(
            feed.viewers.lock().unwrap().list.len(),
            8,
            "no viewer orphaned"
        );
        assert!(!feed.done.load(Ordering::Acquire));
    }

    /// PR 521 review round 2: an admission completing after its feed ended
    /// (the producer ended mid-negotiation) is refused and its attachment
    /// fenced, never handed a dead feed.
    #[test]
    fn an_admission_after_the_feed_closed_is_refused_and_fenced() {
        let r = rig();
        let first = attachment();
        r.rung
            .negotiate("cast-1", &offer(), Arc::clone(&first))
            .unwrap();
        // The producer ends: the run's registry closes.
        let feed_viewers = Arc::clone(&r.rung.feeds().get("cast-1").unwrap().viewers);
        drop(r.registry);
        wait("the feed closed", || feed_viewers.lock().unwrap().closed);
        assert!(first.is_fenced(), "its viewers are fenced");
        // An offer that had reserved the old feed before it closed: the
        // append under the lock sees `closed`.
        let late = attachment();
        {
            let viewers = feed_viewers.lock().unwrap();
            assert!(viewers.closed);
        }
        // A new offer finds no live feed and cannot start one (no producer).
        assert!(r
            .rung
            .negotiate("cast-1", &offer(), Arc::clone(&late))
            .is_err());
        assert!(!feed_viewers
            .lock()
            .unwrap()
            .list
            .iter()
            .any(|v| Arc::ptr_eq(v, &late)));
    }

    /// PR 521 review round 2: a viewer admitted during the linger (after the
    /// worker last saw no viewer) keeps the feed: retirement is decided
    /// under the same lock the admission appends under.
    #[test]
    fn a_viewer_admitted_during_the_linger_keeps_the_feed() {
        let r = rig();
        let a = attachment();
        r.rung
            .negotiate("cast-1", &offer(), Arc::clone(&a))
            .unwrap();
        a.fence();
        std::thread::sleep(FEED_LINGER.saturating_sub(Duration::from_millis(300)));
        let b = attachment();
        r.rung
            .negotiate("cast-1", &offer(), Arc::clone(&b))
            .unwrap();
        std::thread::sleep(Duration::from_millis(800));
        assert!(!b.is_fenced(), "admitted during the linger: kept");
        assert_eq!(r.factory.slots_in_use(), 1, "the same feed serves it");
        b.fence();
        wait("retired after its last viewer", || {
            r.factory.slots_in_use() == 0
        });
    }

    /// A retired feed's late unregister never removes a newer registration.
    #[test]
    fn a_retired_feed_never_unregisters_its_successor() {
        let r = rig();
        let (_, feed) = egress_feed();
        let old = r.handle.register_output_generation("x", 4, feed.clone());
        let new = r.handle.register_output_generation("x", 4, feed);
        r.handle.unregister_output_generation("x", old);
        assert!(
            r.handle.negotiate("x", &offer(), true).is_ok(),
            "the newer one stays"
        );
        r.handle.unregister_output_generation("x", new);
        assert!(r.handle.negotiate("x", &offer(), true).is_err());
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
