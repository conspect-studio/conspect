//! Per-source **runtime audio ingest** — the audio peer of the video decode
//! thread (AUD-2).
//!
//! The pure decode + resample logic lives in [`multiview_audio`] (the
//! [`audio_decode_loop`](multiview_audio::store::audio_decode_loop) over an
//! [`AudioFileDecoder`](multiview_audio::AudioFileDecoder), itself a thin wrapper
//! over the [`multiview_ffmpeg`] safe seam): it opens the container on the worker
//! thread, decodes + resamples to the canonical 48 kHz / stereo / `f32` format,
//! and publishes blocks into a lock-free [`AudioStore`]. This module is the
//! **cli-side supervision**: it maps a configured source to a decodable location
//! and drives the decode loop under the SAME supervised-reconnect bracket the
//! video [`ingest_loop`](crate::pipeline) uses — a live source whose audio drops
//! or EOFs reconnects with capped-exponential, jittered backoff; a finite file
//! plays once and then rides silence (the store silence-fills past EOF, so the
//! sampled track is gap-free — ADR-R005 §4.1).
//!
//! ## It samples, it never paces (invariants #1/#2/#10)
//! The decode thread only ever **writes** the lock-free [`AudioStore`] it shares
//! by `Arc` with the [`ProgramBus`](multiview_audio::ProgramBus). The output
//! clock *samples* that store per tick (a wait-free read that silence-fills any
//! un-written span), so a slow, fast, wedged, or never-ending audio source can
//! neither pace nor stall the output clock (invariant #1) and cannot
//! back-pressure the engine (invariant #10) — exactly like the video tiles.
//!
//! ## Bad inputs are the purpose
//! An open failure, a no-audio source, or a mid-stream decode error logs and
//! ends *this* connection — never the run. A live source then reconnects; a
//! finite/audio-free source rides silence. No decode fault ever crashes or
//! stalls the program (bulletproof continuous output is the product).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_audio::store::{audio_decode_loop, AudioStore};
use multiview_audio::{AudioFormat, ChannelLayout, ToneGenerator};
use multiview_core::time::Rational;

use crate::pipeline::{next_reconnect_attempt, reconnect_backoff, sleep_interruptible, JitterRng};

/// The bounded depth (in frames) of each per-source [`AudioStore`].
///
/// Two seconds at the canonical 48 kHz is ample headroom for the decode thread
/// to run ahead of the output clock's per-tick pull without the ring ever
/// growing: anything beyond this is dropped oldest-first (the data-plane "queues
/// drop, never grow" rule). Mirrors the `96_000` the program-bus unit tests use.
const STORE_CAPACITY_FRAMES: usize = 96_000;

/// The canonical program audio format every per-source store is built at: 48 kHz
/// stereo `f32`. Every source's decode resamples to this BEFORE publishing, so
/// the [`ProgramBus`](multiview_audio::ProgramBus) mixes a single uniform format
/// (streaming-gotchas §5: "per-input resample to 48k fltp BEFORE mixing").
#[must_use]
pub(crate) fn canonical_format() -> AudioFormat {
    AudioFormat::new(AudioFormat::CANONICAL_RATE, ChannelLayout::Stereo)
}

/// Build an empty per-source [`AudioStore`] at the [`canonical_format`], shared
/// (`Arc`) between this source's decode thread (writer) and the program bus
/// (reader). Returned to the pipeline so it can both route the store onto the
/// bus AND hand a clone to the decode thread.
#[must_use]
pub(crate) fn new_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::new(canonical_format(), STORE_CAPACITY_FRAMES))
}

/// Everything one source needs to decode its audio on its own thread: where the
/// audio lives (a libav-openable path or URL) and whether it is a live
/// (never-ending) source.
///
/// Built only for sources whose media libav can open for audio — a local file or
/// a network URL (rtsp/hls/ts/srt/rtmp). Synthetic sources carry no audio; NDI
/// audio is a separate host-memory path (not wired here). A source with no audio
/// stream simply has its decode loop end at open time and rides silence.
pub(crate) struct AudioIngestPlan {
    /// The source id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The libav-openable audio location (a file path or a network URL string).
    pub(crate) location: String,
    /// Whether this is a live (continuous, never-EOF) source: a live source's
    /// audio is reopened on EOF/error (a transient HLS/RTSP audio drop
    /// reconnects); a finite file plays its audio once and then rides silence.
    pub(crate) live: bool,
}

/// Drive a source's audio decode under the supervised-reconnect bracket: open +
/// decode the source's best audio stream into `store` (resampling to the
/// canonical 48 kHz stereo format), reconnecting a *live* source on EOF/error
/// with capped-exponential, jittered backoff until `stop` is raised.
///
/// This is the audio twin of the video
/// [`ingest_loop`](crate::pipeline): the inner per-connection decode is the pure
/// [`audio_decode_loop`](multiview_audio::store::audio_decode_loop) (which opens
/// the `!Send` libav decoder on this worker thread, publishes blocks, and returns
/// on stop/EOF/error); this function wraps it in the reconnect supervision a live
/// transport needs. It only ever WRITES the lock-free store, so it can neither
/// pace nor stall the output clock (invariant #1) nor back-pressure the engine
/// (invariant #10); a wedged libav network call is bounded by the supervisor's
/// detach-on-grace teardown, exactly as for video.
pub(crate) fn audio_ingest_loop(plan: &AudioIngestPlan, store: &AudioStore, stop: &AtomicBool) {
    let mut attempt: u32 = 0;
    let mut jitter = JitterRng::seeded(&plan.id);
    let location = std::path::Path::new(&plan.location);
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        // One connection's worth of decode. `audio_decode_loop` opens the decoder
        // on THIS thread (libav contexts are not `Send`), publishes every decoded
        // block, and returns on stop / EOF / open-or-decode error — logging the
        // fault itself. A no-audio source returns immediately (open finds no audio
        // stream); its tile then simply rides silence.
        audio_decode_loop(location, ChannelLayout::Stereo, store, stop);
        let ran_for = started.elapsed();
        if !plan.live || stop.load(Ordering::Acquire) {
            // A finite source has played its audio out (the store silence-fills
            // past EOF forever); a stop was requested. Either way, this ends.
            return;
        }
        // Live source: escalate from THIS connection's health, then wait the
        // resulting backoff (checking `stop` in slices so teardown stays prompt)
        // and reconnect — the same policy the video ingest uses.
        attempt = next_reconnect_attempt(attempt, ran_for);
        let nap = reconnect_backoff(attempt, jitter.next_unit());
        tracing::debug!(
            source = %plan.id,
            attempt,
            ?nap,
            "reconnecting live source audio after backoff"
        );
        sleep_interruptible(nap, stop);
    }
}

/// Everything a synthetic source needs to publish its **line-up tone** on its own
/// thread (AUD-5): the source id (the program-bus route key + diagnostics) and
/// the output cadence the tone is paced to.
///
/// Built only for the `bars` synthetic source — the SMPTE/EBU colour-bars card's
/// companion is a 1 kHz reference tone; `solid` and `clock` synthetic sources
/// carry no audio and ride silence on the bus.
pub(crate) struct ToneIngestPlan {
    /// The source id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The output cadence the tone is paced to (so the store stays exactly one
    /// per-tick budget ahead of the program bus's pull, gap-free).
    pub(crate) cadence: Rational,
}

/// Target buffered lead (frames) the tone keeps ahead of the bus's read cursor:
/// half a second at the canonical 48 kHz. Comfortably inside the store's
/// [`STORE_CAPACITY_FRAMES`] (2 s) so a top-up never evicts un-read tone, and
/// large enough that a scheduling hiccup on this thread never lets the bus's
/// per-tick pull outrun the published tone (which would read silence — a gap).
const TONE_LEAD_FRAMES: i64 = 24_000;

/// Refill the store whenever the lead drops below this (half the target): a
/// hysteresis band so the thread does a few bounded `sin` bursts rather than a
/// tight spin, then sleeps.
const TONE_REFILL_THRESHOLD_FRAMES: i64 = TONE_LEAD_FRAMES / 2;

/// Publish the synthetic **1 kHz line-up tone** into `store` until `stop` is
/// raised — the audio peer of [`crate::synth::generator_loop`] for the `bars`
/// source (AUD-5).
///
/// The tone is generated by [`multiview_audio::ToneGenerator`] (a pure,
/// phase-continuous sine carrying integer phase, so it never drifts — invariant
/// #3/#6). The loop keeps the store filled to a fixed [`TONE_LEAD_FRAMES`] lead
/// **ahead of the bus's own read cursor** ([`AudioStore::read_cursor`]): it tops
/// the buffer up in one phase-continuous burst whenever the lead falls below the
/// refill threshold, then sleeps. Pacing against the reader's cursor (not
/// wall-clock) makes the tone gap-free regardless of how the bake consumer is
/// paced — the bus's per-tick pull always lands inside published tone, never on
/// an un-written (silence) span.
///
/// If the reader cursor *jumps past* the write head (a `DropOnOverload` catch-up,
/// where the bus's tick-driven [`SampleClock`](multiview_audio::cadence::SampleClock)
/// skips a large span) the loop **seeks the generator forward** to the cursor and
/// resumes from there — it never tries to back-fill the skipped (already-evicted,
/// never-read) span, so every burst stays bounded by [`TONE_LEAD_FRAMES`]
/// (bounded copy cost, no spin). The lead is bounded well inside the store
/// capacity, so the ring never evicts tone the reader has not consumed and never
/// grows (the data-plane "queues drop, never grow" rule).
///
/// It only ever **writes** the lock-free store, so it can neither pace nor stall
/// the output clock (invariant #1) nor back-pressure the engine (invariant #10);
/// a refill is a cheap, bounded per-frame `sin` burst off the hot path. `stop` is
/// polled each wakeup so teardown is prompt.
pub(crate) fn tone_publish_loop(plan: &ToneIngestPlan, store: &AudioStore, stop: &AtomicBool) {
    let format = store.format();
    let mut generator = ToneGenerator::line_up(format);
    // Absolute count of frames published so far (the writer's write head; the
    // store's `read_cursor` lives in the same absolute coordinate space).
    let mut published: i64 = 0;
    let poll = refill_poll_interval(plan.cadence);
    while !stop.load(Ordering::Acquire) {
        let read_cursor = store.read_cursor();
        // If the reader has overrun the write head (a large tick-driven catch-up),
        // skip the generator forward to the cursor rather than back-filling the
        // never-read span — keeps every burst bounded by the lead.
        if read_cursor > published {
            generator.seek_to_frame(u64::try_from(read_cursor).unwrap_or(0));
            published = read_cursor;
        }
        let lead = published.saturating_sub(read_cursor);
        if lead < TONE_REFILL_THRESHOLD_FRAMES {
            // Top up to the target lead in one phase-continuous burst (bounded by
            // `TONE_LEAD_FRAMES` since the overrun case above already realigned).
            let want = TONE_LEAD_FRAMES.saturating_sub(lead).max(0);
            let frames = usize::try_from(want).unwrap_or(0);
            if frames > 0 {
                match generator.next_block(frames) {
                    Ok(block) => {
                        if let Err(error) = store.publish(&block) {
                            // The generator and store share the canonical format, so
                            // a mismatch is a programming error: log and stop rather
                            // than busy-loop. The source then rides silence.
                            tracing::error!(%error, source = %plan.id, "tone publish rejected; stopping");
                            return;
                        }
                        published = published.saturating_add(want);
                    }
                    Err(error) => {
                        tracing::error!(%error, source = %plan.id, "tone generation failed; stopping");
                        return;
                    }
                }
            }
        }
        sleep_interruptible(poll, stop);
    }
}

/// The refill poll interval: ~10 output ticks (clamped to `[5 ms, 200 ms]`). Short
/// enough that the bus never drains the buffered lead between polls, long enough
/// that the thread is mostly asleep. Derived from `cadence` so it scales with the
/// output rate.
#[must_use]
fn refill_poll_interval(cadence: Rational) -> Duration {
    let num = u64::try_from(cadence.num).unwrap_or(1).max(1);
    let den = u64::try_from(cadence.den).unwrap_or(1).max(1);
    // One tick = den/num seconds; ten ticks in nanos = 10 * den * 1e9 / num.
    let nanos = den.saturating_mul(10_000_000_000) / num;
    Duration::from_nanos(nanos.clamp(5_000_000, 200_000_000))
}

/// Everything a **media-player channel** needs to loop its embedded audio on its
/// own thread (ADR-T019 / ADR-0097 §8): where the asset audio lives, the
/// integer-frame vamp window + cadence that defines the loop body (the same
/// [`PlayoutGeometry`](crate::player::PlayoutGeometry) the video uses, so the
/// audio wraps on the same instant), and the wait-free
/// [`PlayerControlBus`](crate::player::PlayerControlBus) the audio rail **samples
/// and follows** (ADR-T019 §1 — the video rail is the sole mailbox consumer and
/// publishes its authoritative transport state here; the audio never drains the
/// mailbox itself, so the rails can never desync).
pub(crate) struct PlayerAudioPlan {
    /// The player channel id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The libav-openable asset audio location (a file path).
    pub(crate) location: String,
    /// The vamp-segment start (asset source frames).
    pub(crate) vamp_in_frames: u64,
    /// The vamp-segment end, exclusive (asset source frames).
    pub(crate) vamp_out_frames: u64,
    /// The asset cadence — the frame↔sample mapping the loop body is sliced by
    /// (so the loop length is the audio duration of `vamp_len` asset frames, the
    /// same sample count at any output cadence — ADR-T019 §3).
    pub(crate) cadence: Rational,
    /// The output cadence (the bus tick rate) the fill loop paces its top-ups to.
    pub(crate) output_cadence: Rational,
    /// The wait-free control bus (the SAME `Arc` on the video rail's
    /// [`PlayerHandle`](crate::player::PlayerHandle)): the audio loop SAMPLES this
    /// each block and follows the video's published transport state, so
    /// vamp/arm-exit/stop land on the same boundary as the video by construction.
    pub(crate) control_bus: Arc<crate::player::PlayerControlBus>,
}

/// Retain **only** the vamp window `[in_sample, want_end)` from a forward stream of
/// decoded interleaved blocks — discarding each block that lies entirely **before**
/// `in_sample` (the pre-`vamp_in` head) and stopping once `want_end` is reached
/// (ADR-T019 §5, the CRITICAL-3 fix).
///
/// `AudioFileDecoder` has no seek, so a late vamp window can only be reached by
/// decoding forward through the head; but the head must NOT be **buffered**.
/// Accumulating every decoded sample from frame 0 into one buffer (then slicing)
/// transiently holds the whole pre-window prefix (peak ≈ asset size). Instead this
/// discards pre-window blocks and appends only the in-window samples, so the **peak
/// resident buffer is the window size** (`want_end − in_sample`), never the asset
/// size — even for a late window in a long asset. The returned buffer starts at
/// `in_sample` (so the caller builds the deck via `with_segment` directly: body =
/// the first `vamp_len` frames, lap-over = the rest).
///
/// Pure (no libav) so it is unit-tested without the `ffmpeg` CLI; the ffmpeg driver
/// [`decode_vamp_window_48k`] feeds it `AudioFileDecoder::next_block` output.
#[cfg(feature = "ffmpeg")]
#[must_use]
fn retain_vamp_window(
    blocks: impl IntoIterator<Item = Vec<f32>>,
    channels: usize,
    in_sample: usize,
    want_end: usize,
) -> Vec<f32> {
    if channels == 0 || want_end <= in_sample {
        return Vec::new();
    }
    let window_frames = want_end - in_sample;
    // Size the retained buffer to EXACTLY the window — it never grows past it, so the
    // pre-`vamp_in` prefix is never resident (the CRITICAL-3 bound).
    let mut out: Vec<f32> = Vec::with_capacity(window_frames.saturating_mul(channels));
    // The absolute frame index of the NEXT decoded sample (advances by each block's
    // frame count). Blocks entirely `< in_sample` are discarded without retention.
    let mut decoded_frames: usize = 0;
    for block in blocks {
        let block_frames = block.len() / channels.max(1);
        if block_frames == 0 {
            continue;
        }
        let block_start = decoded_frames;
        let block_end = block_start.saturating_add(block_frames);
        decoded_frames = block_end;

        // Entirely before the window: discard (do NOT retain the prefix).
        if block_end <= in_sample {
            continue;
        }
        // Entirely at/after the window end: nothing more to retain — stop.
        if block_start >= want_end {
            break;
        }
        // Overlap of [block_start, block_end) with [in_sample, want_end): retain it.
        let from = in_sample.max(block_start);
        let to = want_end.min(block_end);
        let src_from = from.saturating_sub(block_start).saturating_mul(channels);
        let src_to = to.saturating_sub(block_start).saturating_mul(channels);
        if let Some(slice) = block.get(src_from..src_to) {
            out.extend_from_slice(slice);
        }
        if to >= want_end {
            break;
        }
    }
    out
}

/// Decode the asset audio (resampled to canonical 48 kHz stereo `f32`) and return
/// **only** the vamp window `[in_sample, want_end)` (ADR-T019 §5). Drives
/// [`retain_vamp_window`] over `AudioFileDecoder::next_block`, so the pre-`vamp_in`
/// head is decoded-and-discarded, never buffered (peak resident buffer ≈ the window
/// size). Returns an empty `Vec` on open/decode failure or a no-audio source (the
/// player then rides silence — never an error, never a stall).
#[cfg(feature = "ffmpeg")]
#[must_use]
fn decode_vamp_window_48k(path: &std::path::Path, in_sample: usize, want_end: usize) -> Vec<f32> {
    use multiview_audio::AudioFileDecoder;

    let channels = canonical_format().channel_count();
    // `multiview_audio::AudioFileDecoder` resamples to 48 kHz / this layout / f32.
    let mut decoder = match AudioFileDecoder::open(path, ChannelLayout::Stereo) {
        Ok(d) => d,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "media-player audio: open failed; riding silence");
            return Vec::new();
        }
    };
    // A block iterator over the decoder that logs+stops on a mid-stream decode error
    // (the window retains what decoded so far; `from_clip_window`/the caller then
    // refuses a materially-short window). `next_block` yields `Ok(Some)` per block,
    // `Ok(None)` at EOS, `Err` on a decode fault.
    let blocks = std::iter::from_fn(move || match decoder.next_block() {
        Ok(Some(block)) => Some(block.interleaved().to_vec()),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "media-player audio: decode error; using what decoded so far");
            None
        }
    });
    retain_vamp_window(blocks, channels, in_sample, want_end)
}

/// Drive a media-player channel's **looping embedded audio** (ADR-T019): prime a
/// [`LoopDeck`](multiview_audio::LoopDeck) once from the asset's `[vamp_in,
/// vamp_out + W)` window, then keep `store` filled a bounded lead **ahead of the
/// bus's read cursor** with the looped (crossfaded-at-the-seam) stream. Each
/// top-up first **samples the video rail's authoritative
/// [`PlayerControlBus`](crate::player::PlayerControlBus)** (ADR-T019 §1) and
/// follows it — the audio never drains the transport mailbox itself (the video is
/// the sole consumer), so the rails can never desync; `vamp`/`arm-exit`/`stop`
/// land on the same boundary as the video by construction.
///
/// It only ever **writes** the lock-free `store`, so it can neither pace nor stall
/// the output clock (inv #1) nor back-pressure the engine (inv #10) — exactly like
/// the tone loop it mirrors. The fill path uses a **reusable scratch buffer**
/// (`read_into` + `publish_samples`) — no per-block heap allocation (rule 22). A
/// no-audio / failed / over-cap prime yields an empty deck → the channel rides
/// **silence** (the store silence-fills), never an error.
///
/// Unlike [`audio_ingest_loop`], a player channel's audio is **buffer-and-replay**,
/// not a streamed decode: the loop seam is sample-exact and click-free
/// (ADR-T019), which a per-lap container seek (no audio IDR) could not be.
#[cfg(feature = "ffmpeg")]
pub(crate) fn player_audio_loop(plan: &PlayerAudioPlan, store: &AudioStore, stop: &AtomicBool) {
    use multiview_audio::loopdeck::{
        LoopDeck, DEFAULT_CROSSFADE_FRAMES, MAX_LOOP_SECONDS, SAMPLE_RATE,
    };

    let channels = canonical_format().channel_count();
    // Prime (ADR-T019 §5 — CRITICAL-3): decode ONLY the vamp window
    // `[vamp_in, vamp_out + W)` and retain ONLY that span — the pre-`vamp_in` head is
    // decoded-and-discarded, NEVER buffered (peak resident buffer ≈ the window size,
    // not the asset size). REFUSE an over-cap BODY explicitly (a window longer than
    // `MAX_LOOP_SECONDS` rides silence with a logged reason — never a silent clamp
    // that would shift the loop point). The frame→sample map is exact rationals
    // (inv #3). `with_segment` re-checks raggedness; the materially-short / over-cap
    // refusal yields an empty (silent) deck.
    let body_frames = plan.vamp_out_frames.saturating_sub(plan.vamp_in_frames);
    let body_samples = frames_to_samples(body_frames, plan.cadence);
    let xfade = DEFAULT_CROSSFADE_FRAMES;
    let mut deck = if body_samples == 0
        || body_samples > MAX_LOOP_SECONDS.saturating_mul(SAMPLE_RATE)
    {
        if body_samples > 0 {
            tracing::warn!(
                player = %plan.id,
                body_seconds = body_samples / SAMPLE_RATE,
                cap_seconds = MAX_LOOP_SECONDS,
                "media-player audio: vamp window exceeds the loop cap — riding silence (refused, not clamped)"
            );
        }
        LoopDeck::empty(canonical_format())
    } else {
        // The exact-rational sample bounds of the window `[vamp_in, vamp_out + W)`.
        let in_sample = frames_to_samples(plan.vamp_in_frames, plan.cadence);
        let out_sample = frames_to_samples(plan.vamp_out_frames, plan.cadence);
        let in_idx = usize::try_from(in_sample).unwrap_or(usize::MAX);
        let loop_len = usize::try_from(body_samples).unwrap_or(usize::MAX);
        let want_end = usize::try_from(out_sample)
            .unwrap_or(usize::MAX)
            .saturating_add(xfade);
        // Decode-and-discard the head; retain only `[in_sample, want_end)` (starting
        // at `vamp_in`), so the buffer handed to the deck is the window, not the
        // prefix (CRITICAL-3). `from_window` builds the loop over the pre-sliced
        // window, REFUSING (empty/silent deck) a materially-short window rather than
        // looping a shifted-length body, tolerating only a tiny resampler-edge
        // shortfall (ADR-T019 §5). The transient window buffer is freed after.
        let window = decode_vamp_window_48k(std::path::Path::new(&plan.location), in_idx, want_end);
        let was_short = window.len() / channels < loop_len;
        let deck = LoopDeck::from_window(canonical_format(), &window, loop_len, xfade);
        if was_short && deck.loop_frames() == 0 {
            tracing::warn!(
                player = %plan.id,
                "media-player audio: asset shorter than the declared vamp window — riding silence (refused, not clamped)"
            );
        }
        deck
    };

    // The audio rail FOLLOWS the video rail's published transport state (ADR-T019
    // §1/§2.3) — it never drains the mailbox itself. The video publishes the
    // channel's initial state (vamp / play-once) on its first frame, which this rail
    // samples below; the deck starts vamping by default so a boot-vamping player
    // loops from the first block even before that first sample.
    let mut last_control_gen: u64 = 0;

    // CRITICAL-2 (rule 22): a SINGLE reusable scratch buffer for the read path,
    // sized once to the maximum window and filled IN PLACE every block — no
    // per-block heap allocation. `read_into` reuses this `Vec`'s capacity, and
    // `AudioStore::publish_window` swaps in a pooled snapshot (no alloc) — the one
    // copy is `read_into`'s deck→scratch fill, on this sampled decode thread.
    let max_window = usize::try_from(PLAYER_AUDIO_LEAD_FRAMES.max(0)).unwrap_or(0);
    let mut scratch: Vec<f32> = Vec::with_capacity(max_window.saturating_mul(channels));

    let poll = refill_poll_interval(plan.output_cadence);
    while !stop.load(Ordering::Acquire) {
        // Sample the video rail's authoritative control bus (wait-free) FIRST and, on
        // a real transition, apply it to the deck — so the window we (re)publish this
        // block reflects the CURRENT transport state (ADR-T019 §2.3). vamp/arm-exit/
        // stop therefore land on the same boundary as the video by construction.
        follow_video_control(&plan.control_bus, &mut deck, &mut last_control_gen);

        // The publish-horizon contract (ADR-T019 §2.3): REPLACE the unplayed window
        // `[cursor, H)` re-derived from the deck's CURRENT state every block. Because
        // every stale sample lives at ≥ the exit boundary B ≥ the read cursor (B is
        // a FUTURE wrap, lip-synced to the video anchor), the replace overwrites any
        // pre-transition body before the bus reads it — "no sample past the boundary
        // is ever heard" is then true by construction, not by a short-enough
        // lookahead.
        let cursor = store.read_cursor().max(0);
        let cursor_u = u64::try_from(cursor).unwrap_or(0);
        // The horizon: a bounded lookahead, clamped to the deck's silence-settle
        // frame once an exit has fired (so we publish body→fade→silence, nothing
        // past the settle). The deck itself yields silence past the settle, so this
        // clamp is for efficiency; correctness comes from re-deriving each block.
        let lookahead_end = cursor_u.saturating_add(u64::try_from(max_window).unwrap_or(u64::MAX));
        let horizon = match deck.settle_frame() {
            Some(settle) => settle.min(lookahead_end),
            None => lookahead_end,
        };
        if horizon > cursor_u {
            let frames = usize::try_from(horizon - cursor_u)
                .unwrap_or(0)
                .min(max_window);
            if frames > 0 {
                // Fill the reusable scratch from the deck at the absolute cursor (no
                // alloc), then REPLACE the store window with exactly `[cursor, H)`.
                deck.read_into(cursor_u, frames, &mut scratch);
                if let Err(error) = store.publish_window(cursor, &scratch) {
                    tracing::error!(%error, player = %plan.id, "media-player audio publish rejected; stopping");
                    return;
                }
            }
        }
        // Once the exit has fully settled to silence, the store silence-fills the
        // unplayed tail on its own — nothing more to publish until a fresh transport
        // verb (which `follow_video_control` will pick up next wakeup).
        sleep_interruptible(poll, stop);
    }
}

/// Sample the video rail's authoritative [`PlayerControlBus`] and, on a real
/// transition (a higher generation than last applied), apply the published state
/// to the audio [`LoopDeck`] (ADR-T019 §1). The audio rail FOLLOWS the video — it
/// never independently consumes transport verbs, so the two rails can never
/// desync. An armed-exit publish carries the video's media-time anchor, so the
/// audio arms its exit at the **same** next-vamp boundary the video reaches.
#[cfg(feature = "ffmpeg")]
fn follow_video_control(
    bus: &crate::player::PlayerControlBus,
    deck: &mut multiview_audio::LoopDeck,
    last_gen: &mut u64,
) {
    use crate::player::AudioTransport;
    let ctrl = bus.load();
    if ctrl.generation == *last_gen {
        return;
    }
    *last_gen = ctrl.generation;
    match ctrl.state {
        AudioTransport::Vamping => {
            deck.vamp();
            if let Some(anchor) = ctrl.exit_arm_anchor {
                // Arm at the video's anchor → the same next-vamp boundary.
                deck.arm_exit_at(media_time_to_audio_frame(anchor));
            }
        }
        // Paused holds the loop phase (a later vamp resumes mid-loop); Stopped (the
        // video re-cue) latches a re-phase so a fresh vamp restarts from body[0] at
        // the current frame (MAJOR-4). Both contribute silence until the next vamp.
        AudioTransport::Paused => deck.pause(),
        AudioTransport::Stopped => deck.stop(),
    }
}

/// Convert an output media-time to the audio rail's absolute 48 kHz frame index
/// (exact integer rationals, no float — inv #3): `frame = ns × 48000 / 1e9`.
#[cfg(feature = "ffmpeg")]
fn media_time_to_audio_frame(t: multiview_core::time::MediaTime) -> u64 {
    let ns = t.as_nanos().max(0);
    let frame =
        (i128::from(ns) * i128::from(multiview_audio::loopdeck::SAMPLE_RATE)) / 1_000_000_000;
    u64::try_from(frame).unwrap_or(0)
}

/// The exact 48 kHz sample index of asset `frames` at `cadence` = `num/den` fps:
/// `round(frames × 48000 × den / num)`, in `u128` with no float (inv #3 — the
/// audio loop point lands on an exact sample, never a drifting average). Mirrors
/// the deck's internal `frame_to_sample`; used to bound the prime decode and the
/// cap check. A non-positive cadence yields 0 (the player then rides silence).
#[cfg(feature = "ffmpeg")]
fn frames_to_samples(frames: u64, cadence: Rational) -> u64 {
    let Ok(num) = u64::try_from(cadence.num) else {
        return 0;
    };
    let Ok(den) = u64::try_from(cadence.den) else {
        return 0;
    };
    if num == 0 || den == 0 {
        return 0;
    }
    let rate = multiview_audio::loopdeck::SAMPLE_RATE;
    let numer = u128::from(frames)
        .saturating_mul(u128::from(rate))
        .saturating_mul(u128::from(den));
    let denom = u128::from(num);
    let rounded = numer.saturating_add(denom / 2) / denom;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// The bounded lookahead window (frames) the player audio republishes each block —
/// half a second at 48 kHz. Each wakeup the fill loop REPLACES the store window with
/// `[read_cursor, min(read_cursor + this, settle))` re-derived from the deck's
/// current state (ADR-T019 §2.3), so the unplayed tail is always at most this many
/// frames and is corrected on the very next block after any transport transition —
/// the boundary-tight exit is true by construction, not by a short lookahead. It is
/// comfortably inside the store capacity and ≥ one output tick (so the bus's per-tick
/// pull always lands inside the published window — gap-free).
#[cfg(feature = "ffmpeg")]
const PLAYER_AUDIO_LEAD_FRAMES: i64 = 24_000;

#[cfg(all(test, feature = "ffmpeg"))]
mod player_audio_tests {
    //! Integration tests for the media-player audio loop driver (ADR-T019),
    //! `ffmpeg`-gated: each packages an in-memory tone to a lossless WAV via the
    //! `ffmpeg` CLI (LGPL-clean), exactly as the audio decode-thread tests do.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #![allow(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::indexing_slicing
    )]

    use std::f64::consts::PI;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use multiview_audio::store::AudioStore;
    use multiview_core::time::Rational;

    use super::{canonical_format, player_audio_loop, PlayerAudioPlan};
    use crate::player::{AudioTransport, PlayerControlBus};

    const FS: u32 = 48_000;

    fn ffmpeg_cli_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn tone(amp: f64, freq: f64, seconds: f64) -> Vec<f32> {
        let n = (f64::from(FS) * seconds).round() as usize;
        let mut out = Vec::with_capacity(n * 2);
        let w = 2.0 * PI * freq / f64::from(FS);
        for i in 0..n {
            let s = (amp * (w * i as f64).sin()) as f32;
            out.push(s);
            out.push(s);
        }
        out
    }

    fn encode_to_wav(dir: &Path, samples: &[f32]) -> PathBuf {
        let raw = dir.join("clip.f32");
        let wav = dir.join("clip.wav");
        {
            let mut file = std::fs::File::create(&raw).unwrap();
            let mut bytes = Vec::with_capacity(samples.len() * 4);
            for &s in samples {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            file.write_all(&bytes).unwrap();
        }
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-i",
            ])
            .arg(&raw)
            .args(["-c:a", "pcm_s16le"])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg failed to encode the WAV");
        wav
    }

    /// The driver primes from a 1 s tone clip and LOOPS its `[0, 1 s)` vamp window:
    /// after advancing the bus read cursor PAST one clip length, the store still
    /// yields the tone (a one-shot decode would read silence past EOF). The loop
    /// is sample-locked (period == the vamp window's sample count) and the thread
    /// only writes the store — it never paces the (simulated) clock.
    #[test]
    fn player_audio_loops_the_vamp_window_past_one_clip_length() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping player-audio loop test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 1.0 s of a 1 kHz tone (48_000 frames). At 48 fps, frames 0..48 = [0,1 s).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 1.0));

        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        // The control bus defaults to Vamping, so the audio loops from block 0
        // (the video rail would publish the same initial state). No verbs needed.
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "vt1".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // 48 frames @ 48 fps = 1 s = 48_000 samples
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };

        // Run the driver on its own thread (the data-plane peer of the video
        // player thread).
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });

        // Simulate the bus SAMPLING the store at ~real-time: pull tick-sized blocks
        // (1600 frames ≈ one 30 fps tick), pacing the pulls so the driver (which
        // keeps a 0.5 s lead) stays ahead — exactly how the bake consumer samples a
        // per-source store. Advance well past one clip length (1.5 clips) and keep
        // every block read, so the LAST block is past frame 48_000.
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        let mut advanced = 0usize;
        let target = 72_000usize; // 1.5 × the 48_000-frame clip
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        // Let the driver prime + publish its first lead before the bus starts.
        std::thread::sleep(std::time::Duration::from_millis(250));
        while advanced < target && std::time::Instant::now() < deadline {
            let block = store.read(1600);
            blocks.push(block.interleaved().to_vec());
            advanced += 1600;
            // ~33 ms per 1600-frame tick: pace near real-time so the driver refills.
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        assert!(
            advanced >= target,
            "did not advance past one clip length in time (advanced {advanced})"
        );
        // A block well PAST one full clip length (frame ~60_000, deep in lap 2) must
        // carry real tone energy: a one-shot decode would silence-fill there; the
        // LOOP keeps the tone alive (sample-locked, period == the vamp window).
        let late_idx = 60_000 / 1600; // the block covering frame ~60_000
        let late = blocks.get(late_idx).cloned().unwrap_or_default();
        let energy: f64 = late.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy > 1.0,
            "past one clip length the store is silent — the audio did not LOOP (energy {energy:.3})"
        );
    }

    /// Defect 3: a vamp window that does NOT start at the asset head loops
    /// correctly. The window is `[vamp_in=24, vamp_out=48]` (the SECOND half of a
    /// 1 s clip) — the prime must decode through `vamp_out` (not cap from start at
    /// frame 0) and slice the `[0.5 s, 1.0 s)` body. Proves the windowed prime
    /// reaches an offset window (the bug silenced anything past the from-start cap).
    #[test]
    fn a_vamp_window_not_at_the_asset_head_loops_correctly() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping offset-window test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 1.0 s tone (48_000 frames). Vamp the SECOND half: frames 24..48 @ 48 fps
        // = [0.5 s, 1.0 s) = samples 24_000..48_000 (a 24_000-frame loop).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 1.0));
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "offset".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 24,  // 0.5 s
            vamp_out_frames: 48, // 1.0 s → a 0.5 s = 24_000-sample loop body
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Advance past TWO loop bodies (2 × 24_000 = 48_000) — past where a
        // one-shot decode of the offset window would silence-fill.
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        let mut advanced = 0usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        std::thread::sleep(std::time::Duration::from_millis(250));
        while advanced < 48_000 && std::time::Instant::now() < deadline {
            blocks.push(store.read(1600).interleaved().to_vec());
            advanced += 1600;
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
        // A block past one loop body (frame ~36_000, deep in lap 2 of the offset
        // window) carries the tone — the offset window looped (not silence).
        let late = blocks.get(36_000 / 1600).cloned().unwrap_or_default();
        let energy: f64 = late.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy > 1.0,
            "an offset vamp window [0.5 s, 1.0 s) did not loop (energy {energy:.3}) — the prime did not decode through vamp_out"
        );
    }

    /// A player with NO audio stream (a silent asset) rides silence: the driver
    /// primes an empty deck and the store reads silence forever — never a stall,
    /// never a panic. (Here: a non-existent path → open fails → empty deck.)
    #[test]
    fn a_missing_or_silent_asset_rides_silence() {
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "silent".to_owned(),
            location: "/nonexistent/no-such-asset.wav".to_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48,
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Let it run briefly, sampling the store — every read is full silence.
        for _ in 0..5 {
            let block = store.read(1600);
            assert_eq!(
                block.frame_count(),
                1600,
                "silent player still returns full blocks"
            );
            assert!(
                block.interleaved().iter().all(|&s| s == 0.0),
                "a missing/silent asset must ride silence"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
    }

    /// CRITICAL-1 (the root defect), with a REAL lookahead: when an `ArmExit`
    /// arrives AFTER the audio rail has already prepublished ~0.5 s of looped body
    /// into the store, NO published/heard sample past the armed vamp-exit boundary
    /// `B` may carry body energy. The round-2 append-only horizon let the
    /// prepublished body play past `B`; the publish-horizon contract (ADR-T019
    /// §2.3) re-derives + REPLACES the unplayed window from the deck's current
    /// state every block, so the body past `B` is overwritten with fade→silence
    /// before the bus reads it. This fails against the append-only `18039876`.
    #[test]
    fn arm_exit_during_a_real_lookahead_plays_no_body_past_the_boundary() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping arm-exit boundary test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 2 s of a steady 1 kHz tone. Vamp [0, 1 s) → loop L = 48_000 frames. A
        // boundary at B = 48_000 (the lap-0→1 wrap). The lookahead is 0.5 s =
        // 24_000 frames, so with the read cursor parked at 30_000 the lookahead
        // window [30_000, 54_000) straddles B and (pre-fix) holds BODY at [B, 54_000).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 2.0));
        let store = Arc::new(AudioStore::new(canonical_format(), 192_000));
        let stop = Arc::new(AtomicBool::new(false));
        // Boot vamping (no exit armed): the deck loops and the rail prepublishes body.
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "armexit".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // 48 frames @ 48 fps = 1 s = 48_000 samples → L
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });

        // Let it prime + publish the initial lookahead.
        std::thread::sleep(std::time::Duration::from_millis(400));
        // Park the read cursor at 30_000 (inside lap 0, past B − LOOKAHEAD = 24_000)
        // so the next republish covers [30_000, 54_000) — straddling B = 48_000.
        store.seek_to(30_000);
        // Give the fill loop a few poll intervals to publish that BODY lookahead
        // (the stale region [48_000, 54_000) is now body, pre-fix).
        std::thread::sleep(std::time::Duration::from_millis(500));

        // NOW arm the exit. The anchor is the video's media-time at ~frame 30_000;
        // the next vamp wrap strictly after it is B = 48_000. (media-time 30_000/48k
        // s → frame 30_000; next wrap after 30_000 with L=48_000 is 48_000.)
        let anchor_ns = (30_000i64 * 1_000_000_000) / 48_000;
        control_bus.publish(
            AudioTransport::Vamping,
            Some(multiview_core::time::MediaTime::from_nanos(anchor_ns)),
        );
        // Give the fill loop ample time to sample the arm, arm the deck, and
        // REPUBLISH [30_000, 48_000 + xfade) = body→fade→silence — overwriting the
        // stale body — all while the read cursor stays parked (so the republish is
        // guaranteed to land before we read past B).
        std::thread::sleep(std::time::Duration::from_millis(600));
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        // Read the whole post-arm span from the parked cursor and inspect past B.
        store.seek_to(30_000);
        let span = store.read(40_000); // [30_000, 70_000): well past B + the fade
        let s = span.interleaved();
        // The seam fade window: allow [B, B + 1 s_xfade] some energy (the fade tail).
        // The default crossfade is 480 frames; past B + 480 it MUST be silence —
        // no looped body. Frame index within `span` of B = 48_000 − 30_000 = 18_000.
        let xfade = 480usize;
        let past = (18_000 + xfade) * 2; // sample offset past the fade tail
        let tail = &s[past..];
        let energy: f64 = tail.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy < 1e-3,
            "looped body played PAST the armed exit boundary B=48_000 (tail energy {energy:.4}) — \
             the append-only lookahead overshoot (CRITICAL-1) is not fixed"
        );
        // Sanity: BEFORE the boundary there IS body energy (the loop really ran up
        // to B — the test is not vacuously silent).
        let before = &s[..(18_000 * 2)];
        let before_energy: f64 = before.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            before_energy > 1.0,
            "expected real body energy BEFORE the boundary (energy {before_energy:.3}) — test sanity"
        );
    }

    /// CRITICAL-1, the pause/stop half: a `pause` (or `stop`) arriving after the
    /// lookahead was prepublished must NOT let stale body play past the transition.
    /// The republish overwrites the unplayed tail with silence.
    #[test]
    fn pause_after_a_real_lookahead_plays_no_stale_body_past_the_transition() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping pause-overshoot test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 2.0));
        let store = Arc::new(AudioStore::new(canonical_format(), 192_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "pauseover".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48,
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        store.seek_to(20_000);
        std::thread::sleep(std::time::Duration::from_millis(500)); // publish body lookahead
                                                                   // Pause: the unplayed tail [20_000, …) must become silence on the next republish.
        control_bus.publish(AudioTransport::Paused, None);
        std::thread::sleep(std::time::Duration::from_millis(600));
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        store.seek_to(20_000);
        let span = store.read(20_000); // the unplayed tail at pause time
        let energy: f64 = span
            .interleaved()
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum();
        assert!(
            energy < 1e-3,
            "stale looped body played PAST a pause (energy {energy:.4}) — the prepublished \
             lookahead was not revised on the transition (CRITICAL-1)"
        );
    }

    /// MAJOR-4 (end-to-end): the video rail publishing `Stopped` (its `Cued`
    /// re-cue) RE-CUES the audio to head — after a stop, a fresh `Vamping` restarts
    /// the loop from `body[0]` at the current bus position — distinct from a
    /// `Paused` hold. (The pure-deck distinction is covered in the loopdeck tests;
    /// this proves the rail routes `Stopped → deck.stop()` and `Vamping` re-cues.)
    #[test]
    fn the_rail_recues_on_stopped_then_restarts_from_head_on_vamp() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping rail re-cue test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // A 1 s RAMP clip: sample value at frame f is f/48_000 (encodes lap position).
        let mut ramp = vec![0.0f32; 48_000 * 2];
        for f in 0..48_000usize {
            let v = (f as f32) / 48_000.0;
            ramp[f * 2] = v;
            ramp[f * 2 + 1] = v;
        }
        let wav = encode_to_wav(dir.path(), &ramp);
        let store = Arc::new(AudioStore::new(canonical_format(), 192_000));
        let stop = Arc::new(AtomicBool::new(false));
        let control_bus = Arc::new(PlayerControlBus::new());
        let plan = PlayerAudioPlan {
            id: "recue".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // L = 48_000
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus: Arc::clone(&control_bus),
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        // Advance the bus deep into the loop (frame 36_000 → body value 0.75).
        store.seek_to(36_000);
        std::thread::sleep(std::time::Duration::from_millis(300));
        // STOP (the video re-cue): the rail must route this to deck.stop().
        control_bus.publish(AudioTransport::Stopped, None);
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Fresh VAMP: re-cue to head → the loop restarts from body[0]≈0.0 at the
        // CURRENT bus position (frame 36_000), not body[36_000]≈0.75.
        control_bus.publish(AudioTransport::Vamping, None);
        std::thread::sleep(std::time::Duration::from_millis(400));
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        store.seek_to(36_000);
        let head = store.read(1); // first sample after the re-cued vamp
        let v = f64::from(head.interleaved()[0]);
        assert!(
            v.abs() < 0.05,
            "a Stopped (re-cue) + fresh Vamping must restart the loop from body[0]≈0.0 at the \
             current bus frame, got {v:.3} — the rail did not re-cue (MAJOR-4)"
        );
    }

    /// A non-looping player settles the audio to silence: the video rail publishes
    /// a one-shot play (Vamping + exit armed at media-time ZERO), so the audio
    /// plays one lap then goes silent — the bus contribution ends cleanly (the
    /// video tile holds its last frame separately).
    #[test]
    fn a_non_looping_player_settles_audio_to_silence_after_one_lap() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping player-audio settle test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 0.25)); // 0.25 s = 12_000 frames
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        // Simulate the VIDEO rail publishing a one-shot play (Vamping + exit
        // armed at media-time ZERO) — the audio plays one lap then settles silent.
        let control_bus = Arc::new(PlayerControlBus::new());
        control_bus.publish(
            AudioTransport::Vamping,
            Some(multiview_core::time::MediaTime::ZERO),
        );
        let plan = PlayerAudioPlan {
            id: "oneshot".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 12, // 12 frames @ 48 fps = 0.25 s = 12_000 samples
            cadence: Rational::new(48, 1),
            output_cadence: Rational::new(48, 1),
            control_bus,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Advance ~3 clip lengths; after the single lap + exit tail the store must
        // settle to silence.
        let mut advanced = 0usize;
        let mut tail_block = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while advanced < 36_000 && std::time::Instant::now() < deadline {
            let buffered = store.buffered_frames();
            if buffered == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            let take = buffered.min(1600);
            tail_block = store.read(take).interleaved().to_vec();
            advanced += take;
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
        let energy: f64 = tail_block
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum();
        assert!(
            energy < 1e-3,
            "a non-looping player must settle its audio to silence after one lap (tail energy {energy:.5})"
        );
    }

    /// CRITICAL-3: priming a LATE vamp window must NOT transiently buffer the whole
    /// pre-`vamp_in` prefix — the peak resident decode buffer is the WINDOW size,
    /// not the asset size. `retain_vamp_window` discards each decoded block that
    /// lies entirely before `vamp_in_sample` and only retains
    /// `[vamp_in_sample, want_end)`, so its returned buffer's capacity is bounded
    /// by the window, NOT by `vamp_out_sample` from frame 0. (Pure: synthetic
    /// blocks, no ffmpeg CLI — the round-2 accumulate-from-0-then-slice would show a
    /// retained capacity ~= the whole pre-window prefix.)
    #[test]
    fn retain_vamp_window_does_not_buffer_the_pre_vamp_in_prefix() {
        use super::retain_vamp_window;
        let channels = canonical_format().channel_count();
        // Simulate a 12 s asset (576_000 frames) decoded in 0.1 s blocks (4_800
        // frames each), vamping a LATE 1 s window [10 s, 11 s) =
        // samples [480_000, 528_000) plus a 480-frame lap-over (want_end 528_480).
        let block_frames = 4_800usize;
        let asset_frames = 576_000usize;
        let in_sample = 480_000usize;
        let want_end = 528_480usize; // 11 s + 480 lap-over
        let window_len = want_end - in_sample; // 48_480 frames
        let nblocks = asset_frames / block_frames;
        // A block iterator: block k carries frames [k*block, (k+1)*block); each
        // sample value = its absolute frame index (so we can verify WHICH samples
        // were retained).
        let blocks = (0..nblocks).map(|k| {
            let base = k * block_frames;
            let mut v = vec![0.0f32; block_frames * channels];
            for f in 0..block_frames {
                let val = (base + f) as f32;
                v[f * channels] = val;
                v[f * channels + 1] = val;
            }
            v
        });

        let window = retain_vamp_window(blocks, channels, in_sample, want_end);

        // The retained buffer is exactly the window (in samples).
        assert_eq!(
            window.len(),
            window_len * channels,
            "retain_vamp_window must return exactly the [in_sample, want_end) window"
        );
        // The FIRST retained sample is the vamp_in SAMPLE (frame 480_000), proving
        // the prefix was discarded, not retained-then-sliced.
        assert!(
            (window[0] - in_sample as f32).abs() < 0.5,
            "the retained window must start at the vamp_in sample {in_sample} (got {})",
            window[0]
        );
        // The KEY bound: the retained buffer's CAPACITY is the window size, NOT the
        // pre-window prefix (~480_000 frames). The round-2 accumulate-from-0 would
        // balloon capacity to ~want_end. Allow one block of slack for growth.
        let cap_frames = window.capacity() / channels;
        assert!(
            cap_frames <= window_len + block_frames,
            "peak retained buffer {cap_frames} frames ballooned past the window {window_len} \
             (+1 block slack) — the pre-vamp_in prefix was buffered (CRITICAL-3)"
        );
    }
}
