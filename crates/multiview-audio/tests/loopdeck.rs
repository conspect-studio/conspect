//! RED tests for the media-player audio **loop deck** (ADR-T019): sample-exact
//! buffer-and-replay of a vamp segment as an overlap-add loop with a
//! correlation-adaptive crossfade at the seam, positioned by the absolute output
//! frame so it stays sample-locked to the video wrap and survives a realign.
//!
//! These assert the load-bearing correctness the feature exists for, including
//! the three defects the cross-vendor design review caught:
//! - the loop **period is exactly `loop_frames`** for any number of laps and is
//!   `read_at`-pure (the same absolute span is byte-identical however it is
//!   pulled), so a forced realign under load lands inside a faded seam — no
//!   un-crossfaded click (rule 26);
//! - a **decorrelated** seam keeps total **power** flat (equal-power chosen);
//! - a **correlated** seam (a sustained tone across the loop) keeps **amplitude**
//!   flat with no +3 dB swell (linear chosen) — both click-free;
//! - the loop length is the audio duration of the vamp frames at the **asset**
//!   cadence (no frame-vs-tick conflation);
//! - an **armed exit** settles to silence at the next seam, exactly once;
//! - a deck primed with **no audio** rides silence (never a stall / panic).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unreadable_literal
)]

use multiview_audio::format::{AudioFormat, ChannelLayout};
use multiview_audio::loopdeck::LoopDeck;
use proptest::prelude::*;

const RATE: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(RATE, ChannelLayout::Stereo)
}

/// A deterministic pseudo-random stereo buffer of `frames` frames, samples in
/// roughly `[-0.5, 0.5]` — a stand-in for decoded program content with no
/// correlation across the loop seam (the honest worst case for a crossfade).
fn noise(frames: usize, seed: u64) -> Vec<f32> {
    let channels = 2usize;
    let mut state = seed | 1;
    let mut out = vec![0.0f32; frames * channels];
    for slot in &mut out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let unit = (state >> 11) as f64 / (1u64 << 53) as f64; // [0,1)
        *slot = (unit - 0.5) as f32;
    }
    out
}

/// Build a deck over a contiguous decoded buffer of `loop_frames` body frames
/// followed by `xfade` lap-over frames (real content past the loop point). The
/// total buffer is `loop_frames + xfade` frames; the deck loops the first
/// `loop_frames` with the lap-over crossfaded in at the seam.
fn deck_with_lapover(loop_frames: usize, xfade: usize, seed: u64) -> LoopDeck {
    let decoded = noise(loop_frames + xfade, seed);
    LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).expect("valid deck")
}

/// Read `total` frames from the deck starting at absolute frame `start` in
/// `chunk`-frame pulls, concatenating the interleaved output. Asserts every pull
/// is full (gap-free).
fn drain_at(deck: &LoopDeck, start: u64, total: usize, chunk: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(total * deck.format().channel_count());
    let mut done = 0usize;
    while done < total {
        let n = chunk.min(total - done);
        let block = deck.read_at(start + done as u64, n);
        assert_eq!(
            block.frame_count(),
            n,
            "read_at must return exactly the requested frames"
        );
        out.extend_from_slice(block.interleaved());
        done += n;
    }
    out
}

/// Per-frame stereo power (l²+r²) of an interleaved buffer.
fn frame_power(interleaved: &[f32]) -> Vec<f64> {
    interleaved
        .chunks_exact(2)
        .map(|f| f64::from(f[0]) * f64::from(f[0]) + f64::from(f[1]) * f64::from(f[1]))
        .collect()
}

/// Per-frame stereo peak |amplitude| (max |l|,|r|) of an interleaved buffer.
fn frame_amp(interleaved: &[f32]) -> Vec<f64> {
    interleaved
        .chunks_exact(2)
        .map(|f| f64::from(f[0].abs()).max(f64::from(f[1].abs())))
        .collect()
}

/// The largest absolute sample-to-sample step (per channel) over an interleaved
/// span — the click detector. A click is a *discontinuity*: a step far larger
/// than the signal's own intrinsic max step. (Low instantaneous power or a sine's
/// natural zero-crossing is NOT a click; a jump is.)
fn max_step(interleaved: &[f32]) -> f64 {
    let mut prev: Option<(f32, f32)> = None;
    let mut worst = 0.0f64;
    for f in interleaved.chunks_exact(2) {
        if let Some((pl, pr)) = prev {
            worst = worst
                .max(f64::from((f[0] - pl).abs()))
                .max(f64::from((f[1] - pr).abs()));
        }
        prev = Some((f[0], f[1]));
    }
    worst
}

// ----------------------------------------------------------------------------
// 1. read_at is a pure function of the absolute frame (period exactly L, and a
//    forced realign lands inside a faded seam — no un-crossfaded click).
// ----------------------------------------------------------------------------

#[test]
fn read_at_is_a_pure_function_of_the_absolute_frame() {
    // The looped stream MUST be a pure function of the absolute output frame, so
    // the same absolute span read in one big pull == read in 1-frame pulls ==
    // read with an injected forward jump (the bus-cursor realign under load).
    // Any chunk/cursor-dependent state would click at the realign seam (rule 26).
    let loop_frames = 500usize;
    let xfade = 64usize;
    let deck = deck_with_lapover(loop_frames, xfade, 0x55AA);

    let start = 0u64;
    let total = loop_frames * 3 + 137; // non-lap-aligned
    let big = drain_at(&deck, start, total, total);
    let small = drain_at(&deck, start, total, 1);
    let medium = drain_at(&deck, start, total, 100);
    assert_eq!(
        big, small,
        "1-frame pulls must equal one big pull (read_at purity)"
    );
    assert_eq!(big, medium, "100-frame pulls must equal one big pull");

    // A FORCED REALIGN: read a window that starts mid-lap-2 (an absolute frame
    // jump past a couple of seams, as a bus catch-up would do). The bytes must
    // equal the corresponding slice of the contiguous `big` stream — i.e. the
    // seam at that absolute position is correctly faded, not skipped.
    let jump_start = (loop_frames * 2 + (loop_frames - xfade / 2)) as u64; // mid-seam of lap 2
    let len = loop_frames; // spans the lap-2→3 seam
    let jumped = drain_at(&deck, jump_start, len, 32);
    let reference = drain_at(&deck, jump_start, len, len);
    assert_eq!(
        jumped, reference,
        "a realign to an absolute mid-seam frame must emit the SAME faded seam (no un-crossfaded click under load)"
    );
}

#[test]
fn the_loop_period_is_exactly_loop_frames_across_many_laps() {
    let loop_frames = 1000usize;
    let xfade = 64usize;
    let deck = deck_with_lapover(loop_frames, xfade, 0xABCD);
    let laps = 5usize;
    let out = drain_at(&deck, 0, loop_frames * laps, 333);
    let ch = 2usize;
    // The CLEAN MIDDLE of every lap, `[xfade, loop_frames)`, is `body[m]` (one lap
    // contributes), so it repeats identically every `loop_frames` — proving the
    // period is exactly `loop_frames` (sample-locked, no drift).
    for lap in 1..laps {
        for m in xfade..loop_frames {
            assert_eq!(
                out[m * ch],
                out[(lap * loop_frames + m) * ch],
                "clean-middle frame {m} drifted at lap {lap} — period must be exactly loop_frames"
            );
        }
    }
}

// ----------------------------------------------------------------------------
// 2a. Decorrelated seam → equal-power → total POWER stays flat (no dip/bump).
// ----------------------------------------------------------------------------

#[test]
fn a_decorrelated_seam_keeps_total_power_flat() {
    // Constant-RMS uncorrelated content: every frame has power 2a². With a
    // decorrelated lap-over (independent noise) the deck must pick EQUAL-POWER, so
    // the seam power stays ≈ 2a² (a linear fade would dip ~50%).
    let loop_frames = 800usize;
    let xfade = 128usize;
    let a = 0.4f32;
    let channels = 2usize;
    let mk = |seed: u64| {
        let mut state = seed | 1;
        let n = (loop_frames + xfade) * channels;
        let mut v = vec![0.0f32; n];
        for slot in &mut v {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = if state & 1 == 0 { a } else { -a };
        }
        v
    };
    let decoded = mk(0x1234_5678);
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();

    let out = drain_at(&deck, 0, loop_frames * 2, loop_frames);
    let power = frame_power(&out);
    let nominal = 2.0 * f64::from(a) * f64::from(a);
    // Seam of lap 0 is `[0, xfade)` (the overlap-add region at the wrap from the
    // previous lap's tail into this lap's head). Examine the lap-1 seam which has a
    // full previous lap behind it.
    let seam_start = loop_frames; // absolute frame of lap-1's wrap seam = [L, L+xfade)
    let seam = &power[seam_start..seam_start + xfade];
    let avg: f64 = seam.iter().sum::<f64>() / xfade as f64;
    let ratio = avg / nominal;
    assert!(
        (0.85..=1.15).contains(&ratio),
        "decorrelated seam mean power ratio {ratio:.3} must stay ≈ 1 (equal-power, no dip/bump)"
    );
    // (For DEcorrelated ±a content the per-sample step is already ~2a everywhere —
    // white noise is maximally discontinuous, so a hard cut at the wrap is NOT
    // distinguishable from the crossfade by step size. The flat MEAN POWER above is
    // the meaningful equal-power criterion here; the step/continuity criterion is
    // exercised by the CORRELATED tonal seam test, where a hard cut WOULD jump.)
}

// ----------------------------------------------------------------------------
// 2b. Correlated seam → linear → AMPLITUDE stays flat (no +3 dB swell/click).
//     THIS is the defect the review caught: equal-power swells √2 on a tone.
// ----------------------------------------------------------------------------

#[test]
fn a_correlated_tonal_seam_does_not_swell_in_amplitude() {
    // A continuous sine across the loop point: body is N cycles of a tone, and the
    // lap-over continues the SAME phase-locked tone (so tail and head are
    // correlated/in-phase). An equal-power crossfade would swell amplitude to
    // √2 ≈ 1.41× at the seam midpoint (an audible per-lap level pump). The deck
    // must detect the correlation and use a LINEAR fade → flat amplitude.
    let channels = 2usize;
    let cycles = 40usize;
    let period = 100usize; // samples per cycle
    let loop_frames = cycles * period; // whole number of cycles → seamless tone
    let xfade = period; // one cycle of crossfade
    let amp = 0.7f32;
    // Build body + lap-over as ONE continuous tone over (loop_frames + xfade)
    // samples (so the lap-over is the genuine continuation of the body's tone).
    let total = loop_frames + xfade;
    let mut decoded = vec![0.0f32; total * channels];
    for i in 0..total {
        let s = amp * (2.0 * std::f32::consts::PI * (i as f32) / (period as f32)).sin();
        decoded[i * channels] = s;
        decoded[i * channels + 1] = s;
    }
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();

    let out = drain_at(&deck, 0, loop_frames * 2, loop_frames);
    let ampl = frame_amp(&out);
    // The seam at lap-1's wrap: peak amplitude must NOT exceed the tone's own peak
    // by more than a small margin (a linear fade of a continuous tone holds the
    // envelope; equal-power would reach ~1.41× = an obvious swell). We bound the
    // ENVELOPE: the per-frame peak over the seam stays under 1.12× the tone peak.
    let seam_start = loop_frames;
    let seam = &ampl[seam_start..seam_start + xfade];
    let peak = seam.iter().copied().fold(0.0f64, f64::max);
    assert!(
        peak <= 1.12 * f64::from(amp),
        "correlated tonal seam peak amplitude {peak:.4} swelled past the tone peak {amp} — equal-power was wrongly used (the +3 dB loop-pump defect)"
    );
    // No CLICK: the seam's largest sample step must stay near the tone's own max
    // step (amp·2π/period, the slope at a zero-crossing). A linear crossfade of a
    // continuous tone holds the envelope, so no discontinuity appears. (A dip to a
    // low instantaneous amplitude is just the sine's natural zero-crossing — NOT a
    // click; only a step jump is.)
    let seam_samples = &out[seam_start * channels..(seam_start + xfade) * channels];
    let step = max_step(seam_samples);
    let tone_step = f64::from(amp) * 2.0 * std::f64::consts::PI / (period as f64);
    assert!(
        step <= 1.5 * tone_step + 1e-6,
        "correlated tonal seam max step {step:.5} exceeds ~1.5× the tone's own step {tone_step:.5} — a discontinuity (click)"
    );
}

// ----------------------------------------------------------------------------
// 3. Loop length is the audio duration of the vamp frames at the ASSET cadence
//    — NOT a SampleClock::total_at tick delta (no frame-vs-tick conflation).
// ----------------------------------------------------------------------------

#[test]
fn loop_length_is_the_decoded_sample_count_independent_of_output_cadence() {
    // The deck is built from a decoded sample buffer; its loop length is exactly
    // the body frame count it was given — there is no cadence in the deck at all,
    // so it cannot conflate asset frames with output ticks. A 24 fps asset vamp of
    // 240 frames is 10 s of audio = 480_000 frames at 48 kHz, REGARDLESS of the
    // output (program) cadence. We assert the deck's reported loop length equals
    // the body frames it was constructed with.
    let loop_frames = 480_000usize; // 10 s at 48 kHz (e.g. 240 frames @ 24 fps)
    let xfade = 480usize; // 10 ms
                          // Build a tiny-amplitude buffer (content irrelevant; only the length matters).
    let decoded = vec![0.01f32; (loop_frames + xfade) * 2];
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    assert_eq!(
        deck.loop_frames(),
        loop_frames,
        "the loop length is the decoded body sample count — no total_at tick delta, no cadence conflation"
    );
}

#[test]
fn from_clip_window_slices_the_vamp_segment_at_the_asset_cadence() {
    use multiview_core::time::Rational;

    // A whole decoded clip of 2 s @ 48 kHz = 96_000 frames. The vamp window is
    // asset frames [vamp_in, vamp_out) at the ASSET cadence; the deck must slice
    // the correct SAMPLE range = the audio duration of those frames, and the loop
    // length must be (vamp_out - vamp_in) frames in seconds × 48_000 — the same
    // sample count at EVERY cadence (no frame-vs-tick conflation).
    let channels = 2usize;
    let clip_frames = 96_000usize; // 2 s
                                   // A ramp so we can verify WHICH samples were sliced (sample i has value i/clip).
    let mut clip = vec![0.0f32; clip_frames * channels];
    for i in 0..clip_frames {
        let v = (i as f32) / (clip_frames as f32);
        clip[i * channels] = v;
        clip[i * channels + 1] = v;
    }

    // 24 fps asset: frames 12..36 = [0.5 s, 1.5 s) = samples 24_000..72_000 (1 s).
    let cad24 = Rational::new(24, 1);
    let deck24 = LoopDeck::from_clip_window(stereo(), &clip, 12, 36, cad24, 0).unwrap();
    assert_eq!(
        deck24.loop_frames(),
        48_000,
        "24 frames @ 24 fps = 1 s = 48_000 samples"
    );
    // Read a CLEAN-MIDDLE frame (past the default crossfade window, so it is
    // body[m] verbatim, not a seam blend): body frame 1000 = clip sample
    // 24_000 + 1000 = 25_000 → value 25000/96000 ≈ 0.2604, proving the body was
    // sliced from the vamp_in SAMPLE (asset-frame 12 @ 24 fps = sample 24_000).
    let mid = deck24.read_at(1000, 1);
    let expect = 25_000.0 / 96_000.0;
    assert!(
        (f64::from(mid.interleaved()[0]) - expect).abs() < 1e-3,
        "the deck must slice the body from the vamp_in SAMPLE 24_000 (got body[1000]={}, expected {expect:.4})",
        mid.interleaved()[0]
    );

    // 25 fps asset: frames 0..50 = [0 s, 2 s) = samples 0..96_000 — DIFFERENT
    // frame indices, but the SAME total_at-free sample math. 50 frames @ 25 fps =
    // 2 s = 96_000 samples (the whole clip).
    let cad25 = Rational::new(25, 1);
    let deck25 = LoopDeck::from_clip_window(stereo(), &clip, 0, 50, cad25, 0).unwrap();
    assert_eq!(
        deck25.loop_frames(),
        96_000,
        "50 frames @ 25 fps = 2 s = 96_000 samples"
    );
}

#[test]
fn read_into_reuses_the_caller_buffer_no_per_block_allocation() {
    // Defect 2 (rule 22): the hot read path must not allocate per block. `read_into`
    // fills a caller-provided buffer in place; once sized to the max refill, its
    // capacity (and backing allocation) is STABLE across reads — no realloc.
    let loop_frames = 1000usize;
    let xfade = 64usize;
    let deck = deck_with_lapover(loop_frames, xfade, 0xF00D);
    let mut buf: Vec<f32> = Vec::with_capacity(1600 * 2);
    let cap0 = buf.capacity();
    let ptr0 = buf.as_ptr();

    // Many reads of the same (and smaller) size: the buffer is reused, never grown.
    for lap in 0..50u64 {
        deck.read_into(lap * 1600, 1600, &mut buf);
        assert_eq!(
            buf.len(),
            1600 * 2,
            "read_into fills exactly frames×channels"
        );
        assert_eq!(
            buf.capacity(),
            cap0,
            "capacity is stable — no realloc on the read path"
        );
        assert_eq!(
            buf.as_ptr(),
            ptr0,
            "the backing allocation is reused (no per-block alloc)"
        );
    }

    // A smaller read reuses the same (larger) allocation too — never shrinks/reallocs.
    deck.read_into(0, 800, &mut buf);
    assert_eq!(buf.len(), 800 * 2);
    assert_eq!(
        buf.capacity(),
        cap0,
        "a smaller read keeps the larger reused allocation"
    );
    assert_eq!(buf.as_ptr(), ptr0);

    // And read_into produces the SAME samples as read_at (the allocating sibling).
    let viaalloc = deck.read_at(12_345, 500);
    let mut viabuf: Vec<f32> = Vec::with_capacity(500 * 2);
    deck.read_into(12_345, 500, &mut viabuf);
    assert_eq!(
        viabuf,
        viaalloc.interleaved(),
        "read_into == read_at samples"
    );
}

#[test]
fn from_clip_window_refuses_a_materially_short_clip_rather_than_silently_clamping() {
    use multiview_core::time::Rational;
    // The declared vamp window is 1 s (48_000 samples) but the decoded clip is
    // only 0.5 s (24_000 samples). Silently clamping the loop body to the
    // available 24_000 frames would SHIFT the loop length (the audio would wrap at
    // 0.5 s while the video wraps at 1 s — a desync). The deck must REFUSE (build
    // an empty/silent deck), not loop a shifted-length body (ADR-T019 §3 / Defect 3).
    let channels = 2usize;
    let clip = vec![0.4f32; 24_000 * channels]; // 0.5 s
    let cad = Rational::new(48, 1);
    let deck = LoopDeck::from_clip_window(stereo(), &clip, 0, 48, cad, 480).unwrap();
    assert_eq!(
        deck.loop_frames(),
        0,
        "a clip materially shorter than the declared vamp window must ride silence (empty deck), NOT loop a shifted-length body"
    );
    // A tiny shortfall (resampler edge — a few samples) is still tolerated: a
    // 48_000-frame window with 47_990 decoded loops the full declared length.
    let near = vec![0.4f32; 47_990 * channels];
    let deck_near = LoopDeck::from_clip_window(stereo(), &near, 0, 48, cad, 480).unwrap();
    assert_eq!(
        deck_near.loop_frames(),
        48_000,
        "a few-sample resampler-edge shortfall is tolerated (loops the full declared length, sample-locked to the video)"
    );
}

#[test]
fn from_clip_window_refuses_an_over_cap_window_explicitly() {
    use multiview_audio::loopdeck::{MAX_LOOP_SECONDS, SAMPLE_RATE};
    use multiview_core::time::Rational;
    // A vamp window whose BODY exceeds MAX_LOOP_SECONDS must be REFUSED explicitly
    // (empty deck → ride silence), never silently clamped to a shifted shorter
    // loop (ADR-T019 §5 / Defect 3). At 30 fps, MAX_LOOP_SECONDS*30 + 30 frames is
    // just over the cap.
    let cad = Rational::new(30, 1);
    let over = MAX_LOOP_SECONDS.saturating_mul(30).saturating_add(30);
    // The clip buffer is irrelevant (the cap is checked before slicing) — a small
    // stub suffices; the cap refusal must fire regardless.
    let clip = vec![0.1f32; 1000 * 2];
    let deck = LoopDeck::from_clip_window(stereo(), &clip, 0, over, cad, 0).unwrap();
    assert_eq!(
        deck.loop_frames(),
        0,
        "an over-cap vamp window must be refused (empty/silent), not clamped to a shifted loop"
    );
    // Sanity: the cap is ~MAX_LOOP_SECONDS of audio.
    assert!(MAX_LOOP_SECONDS.saturating_mul(SAMPLE_RATE) > 0);
}

// ----------------------------------------------------------------------------
// 4. Armed exit: settles to silence at the next seam, exactly once.
// ----------------------------------------------------------------------------

#[test]
fn an_armed_exit_settles_to_silence_at_the_next_seam_exactly_once() {
    let loop_frames = 600usize;
    let xfade = 60usize;
    // A constant non-silent body: while looping output is ~0.3; after the exit
    // settles it must be exactly silence.
    let decoded = vec![0.3f32; (loop_frames + xfade) * 2];
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();

    // Consume partway into lap 0, then arm the exit (the audio thread would do this
    // when it drains an ArmExit verb from the shared mailbox).
    let _ = drain_at(&deck, 0, loop_frames / 2, 64);
    deck.arm_exit();

    // The exit fires at the NEXT seam (the lap-0 wrap at absolute frame L). Read
    // from there for a few laps: after the exit fade the deck must be SILENT.
    let after = drain_at(&deck, (loop_frames / 2) as u64, loop_frames * 3, 64);
    let tail = &after[after.len() - loop_frames * 2..];
    assert!(
        tail.iter().all(|&s| s == 0.0),
        "after the armed exit fires at the seam the deck settles to silence (the bus contribution ends)"
    );
    assert!(
        deck.has_ended(),
        "an armed-exit deck reports ended once the boundary has fired"
    );
}

#[test]
fn cancel_exit_keeps_looping_forever() {
    let loop_frames = 400usize;
    let xfade = 40usize;
    let decoded = vec![0.25f32; (loop_frames + xfade) * 2];
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();
    deck.arm_exit();
    deck.cancel_exit();
    let out = drain_at(&deck, 0, loop_frames * 4, 77);
    let tail = &out[out.len() - loop_frames..];
    assert!(
        tail.iter().any(|&s| s != 0.0),
        "a cancelled exit keeps the loop running (never goes silent)"
    );
    assert!(!deck.has_ended(), "a cancelled-exit deck has not ended");
}

// ADR-T019 / cross-vendor review Finding 2: the exit fade-out tail must scale the
// CONTINUING loop content (frames m, m+1, …) by the cosine envelope — not hold a single
// frame (body[0]) under it. A *constant* body hides the defect (a held frame equals the
// continuation), which is why `an_armed_exit_settles_to_silence` (a constant 0.3 body)
// passes either way. This uses distinct per-frame content and checks the exit tail
// against the loop's OWN continuation scaled by the content-independent fade curve.
#[test]
fn an_exit_fade_attenuates_the_continuing_content_not_a_held_frame() {
    let loop_frames = 600usize;
    let xfade = 60usize;
    let total = loop_frames + xfade;

    // (1) Derive the fade curve g(j) from a CONSTANT deck, where held == continuation so
    // the exit tail is unambiguously C·g(j) whether or not the defect is present.
    let c = 0.5f32;
    let flat = vec![c; total * 2];
    let mut flat_deck = LoopDeck::with_segment(stereo(), &flat, loop_frames, xfade).unwrap();
    flat_deck.vamp();
    let _ = drain_at(&flat_deck, 0, loop_frames / 2, 64);
    flat_deck.arm_exit();
    let flat_tail = flat_deck.read_at(loop_frames as u64, xfade); // exit tail over [L, L+xfade)
    let gain: Vec<f64> = (0..xfade)
        .map(|j| f64::from(flat_tail.interleaved()[j * 2]) / f64::from(c))
        .collect();

    // (2) A strictly increasing RAMP body: every frame differs, so a held first frame is
    // distinguishable from the true continuation across the whole tail window.
    let mut ramp = vec![0.0f32; total * 2];
    for f in 0..total {
        let v = 0.2f32 + 0.6f32 * (f as f32 / total as f32); // [0.2, 0.8), strictly increasing
        ramp[f * 2] = v;
        ramp[f * 2 + 1] = v;
    }
    // The looping continuation over [L, L+xfade) on a deck that is NOT exiting.
    let mut cont_deck = LoopDeck::with_segment(stereo(), &ramp, loop_frames, xfade).unwrap();
    cont_deck.vamp();
    let cont = cont_deck.read_at(loop_frames as u64, xfade);
    // The exit tail over the same span on an armed deck.
    let mut exit_deck = LoopDeck::with_segment(stereo(), &ramp, loop_frames, xfade).unwrap();
    exit_deck.vamp();
    let _ = drain_at(&exit_deck, 0, loop_frames / 2, 64);
    exit_deck.arm_exit();
    let tail = exit_deck.read_at(loop_frames as u64, xfade);

    let cont_s = cont.interleaved();
    let tail_s = tail.interleaved();
    let mut max_err = 0.0f64; // |actual − continuation·g|  → 0 iff the tail fades the continuation
    let mut ref_gap = 0.0f64; // |continuation·g − held·g|  → proves held ≠ continuation for this ramp
    for j in 0..xfade {
        let expected = f64::from(cont_s[j * 2]) * gain[j]; // continuation × fade  (correct)
        let held = f64::from(cont_s[0]) * gain[j]; // body[0]   × fade  (the defect)
        let actual = f64::from(tail_s[j * 2]);
        max_err = max_err.max((actual - expected).abs());
        ref_gap = ref_gap.max((expected - held).abs());
    }
    assert!(
        ref_gap > 1e-2,
        "non-vacuous: the continuation and held-frame forms diverge for this ramp (ref_gap={ref_gap})"
    );
    assert!(
        max_err < 1e-4,
        "the exit fade must attenuate the CONTINUING loop content, not hold body[0] under the envelope (max_err={max_err})"
    );
}

// ----------------------------------------------------------------------------
// 5. Empty / paused / stopped decks: silence, never a stall or panic.
// ----------------------------------------------------------------------------

#[test]
fn an_empty_deck_rides_silence_gap_free() {
    // A player whose asset has no audio (or a failed/over-cap prime) builds an
    // empty deck: it produces exactly `frames` frames of silence on every read,
    // forever — never a short block, never a panic (hold-last-good / never off-air).
    let deck = LoopDeck::empty(stereo());
    for tick in 0..10u64 {
        let block = deck.read_at(tick * 1601, 1601);
        assert_eq!(
            block.frame_count(),
            1601,
            "empty deck still returns full blocks"
        );
        assert!(
            block.interleaved().iter().all(|&s| s == 0.0),
            "empty deck returns silence"
        );
    }
}

#[test]
fn pause_contributes_silence_and_stop_recues() {
    let loop_frames = 300usize;
    let xfade = 30usize;
    let decoded = vec![0.5f32; (loop_frames + xfade) * 2];
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();
    let _ = drain_at(&deck, 0, 100, 50);
    deck.pause();
    // Paused: the bus contribution is silence (not a frozen DC sample, which would
    // click on resume); the video tile holds the picture separately.
    let paused = drain_at(&deck, 100, loop_frames, 50);
    assert!(
        paused.iter().all(|&s| s == 0.0),
        "a paused deck contributes silence to the bus"
    );
}

// ----------------------------------------------------------------------------
// MAJOR-4: pause HOLDS the loop phase (resume continues mid-loop); stop RE-CUES
// to the head (a fresh vamp restarts the segment from body[0]). The two MUST be
// observably distinct — the round-2 map sent the video re-cue to pause-in-place.
// ----------------------------------------------------------------------------

#[test]
fn stop_then_vamp_recues_to_head_distinct_from_pause_resume() {
    // A body that is a RAMP within the lap (sample value encodes the lap position)
    // so we can read off WHICH lap position the deck resumes at. body[m] = m/L for
    // frame m in [xfade, L) (clean middle); the first sample after a re-cue at the
    // head must be ~0 (body[0]); a pause-resume must continue near where it left.
    let loop_frames = 1000usize;
    let xfade = 0usize; // no seam blend so body[m] is verbatim — clean ramp readout
    let channels = 2usize;
    let mut decoded = vec![0.0f32; loop_frames * channels];
    for m in 0..loop_frames {
        let v = (m as f32) / (loop_frames as f32);
        decoded[m * channels] = v;
        decoded[m * channels + 1] = v;
    }
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();

    // --- PAUSE then VAMP: resume HOLDS phase (continues at the same lap position).
    // Play up to absolute frame 700 (lap position 700 → body value 0.700).
    let _ = deck.read_at(0, 700);
    deck.pause();
    deck.vamp(); // resume
                 // The next read at absolute frame 700 (a pause holds phase: position = 700%L=700).
    let resumed = deck.read_at(700, 1);
    assert!(
        (f64::from(resumed.interleaved()[0]) - 0.700).abs() < 0.02,
        "pause→vamp must HOLD the loop phase (resume near body[700]≈0.700, got {})",
        resumed.interleaved()[0]
    );

    // --- STOP then VAMP: re-cue to HEAD (next read restarts at body[0]≈0.0),
    // regardless of the absolute frame the bus is at.
    let mut deck2 = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck2.vamp();
    let _ = deck2.read_at(0, 700); // advance to absolute frame 700
    deck2.stop(); // re-cue (silence until a fresh vamp)
    deck2.vamp(); // fresh vamp → restart from the head at the CURRENT absolute frame
                  // The very next read (at absolute frame 700) must be body[0]≈0.0 — the loop
                  // was re-phased so the head plays now, NOT body[700].
    let recued = deck2.read_at(700, 1);
    assert!(
        f64::from(recued.interleaved()[0]).abs() < 0.02,
        "stop→vamp must RE-CUE to the head (next sample ≈ body[0]≈0.0, got {}) — distinct from pause",
        recued.interleaved()[0]
    );
    // And it then loops forward from the head: frame 700+250 reads body[250]≈0.250.
    let later = deck2.read_at(700 + 250, 1);
    assert!(
        (f64::from(later.interleaved()[0]) - 0.250).abs() < 0.02,
        "after a re-cue the loop runs forward from the head (body[250]≈0.250, got {})",
        later.interleaved()[0]
    );
}

// ----------------------------------------------------------------------------
// 6. Property tests — period integrity + both seam-correlation regimes.
// ----------------------------------------------------------------------------

/// `SplitMix64` — a strong finalizing mixer used to derive well-distributed,
/// mutually-independent sub-seeds for the ensemble below. Unlike `seed ^ (i·k)`,
/// its avalanche breaks the linear relationship the content xorshift would otherwise
/// preserve between sub-seeds, so the realizations it seeds are effectively
/// independent (verified empirically — see `decorrelated_seam_power_ratio_mean`).
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One decorrelated-content realization's seam-power ratio: the mean per-frame
/// stereo power over the `xfade`-frame crossfade seam, divided by the nominal
/// `2·a²`. Content is ±`a` decorrelated noise seeded by `seed`. For a constant-power
/// crossfade of decorrelated content the expected ratio is exactly 1, but a single
/// realization's estimate has relative SD ≈ 0.50/√xfade (it averages only `xfade`
/// frames), so at small `xfade` one draw is a noisy estimate — see
/// [`decorrelated_seam_power_ratio_mean`].
fn decorrelated_seam_power_ratio(loop_frames: usize, xfade: usize, seed: u64) -> f64 {
    let a = 0.35f32;
    let nominal = 2.0 * f64::from(a) * f64::from(a);
    let mut state = seed | 1;
    let n = (loop_frames + xfade) * 2;
    let mut decoded = vec![0.0f32; n];
    for slot in &mut decoded {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = if state & 1 == 0 { a } else { -a };
    }
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    let out = drain_at(&deck, 0, loop_frames * 2, loop_frames);
    let power = frame_power(&out);
    let seam = &power[loop_frames..loop_frames + xfade];
    seam.iter().sum::<f64>() / xfade as f64 / nominal
}

/// The seam-power ratio averaged over `k` independent decorrelated realizations,
/// each seeded by a `splitmix64`-mixed sub-seed of `seed` (whose avalanche makes the
/// realizations effectively independent — a plain `seed ^ i·k` would stay linearly
/// related through the content xorshift and not decorrelate). The single-realization
/// SD ≈ 0.50/√xfade shrinks ≈1/√k with the ensemble size (measured, no floor), so the
/// callers' `k = ⌈512/xfade⌉` (xfade·k ≥ 512) makes the tight ±20% band a measured
/// ~7σ bound at every `xfade` — testing the *expected* power preservation instead of
/// one high-variance small-`xfade` draw. Independence + the margin are verified
/// empirically by `ensemble_mean_variance_shrinks_with_k`.
fn decorrelated_seam_power_ratio_mean(
    loop_frames: usize,
    xfade: usize,
    seed: u64,
    k: usize,
) -> f64 {
    (0..k)
        .map(|i| {
            let s = splitmix64(seed.wrapping_add(i as u64));
            decorrelated_seam_power_ratio(loop_frames, xfade, s)
        })
        .sum::<f64>()
        / k as f64
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any loop length and crossfade window (clamped ≤ L/2 by the deck), the
    /// clean middle `[xfade, L)` of every lap equals lap 0's — period exactly L.
    #[test]
    fn prop_period_is_exactly_loop_frames(
        loop_frames in 64usize..2048,
        xfade_req in 0usize..4096,
        seed in any::<u64>(),
    ) {
        let xfade = xfade_req.min(loop_frames / 2);
        let deck = deck_with_lapover(loop_frames, xfade, seed ^ 0x9E3779B97F4A7C15);
        let laps = 4usize;
        let out = drain_at(&deck, 0, loop_frames * laps, loop_frames.max(1));
        let ch = 2usize;
        for lap in 1..laps {
            for m in xfade..loop_frames {
                prop_assert_eq!(
                    out[m * ch],
                    out[(lap * loop_frames + m) * ch],
                    "period drift at lap {} frame {}", lap, m
                );
            }
        }
    }

    /// read_at is pure: any absolute span is identical read whole vs in 1-frame
    /// pulls (so a realign under load never skips a faded seam).
    #[test]
    fn prop_read_at_is_pure(
        loop_frames in 64usize..1024,
        xfade_req in 1usize..2048,
        start in 0u64..100_000,
        len in 1usize..2000,
        seed in any::<u64>(),
    ) {
        let xfade = xfade_req.min(loop_frames / 2);
        let deck = deck_with_lapover(loop_frames, xfade, seed);
        let whole = drain_at(&deck, start, len, len);
        let ones = drain_at(&deck, start, len, 1);
        prop_assert_eq!(whole, ones);
    }

    /// A decorrelated equal-RMS seam keeps mean power within band (equal-power).
    #[test]
    fn prop_decorrelated_seam_power_flat(
        loop_frames in 128usize..2048,
        xfade_req in 8usize..4096,
        seed in any::<u64>(),
    ) {
        let xfade = xfade_req.min(loop_frames / 2).max(1);
        // Estimate the EXPECTED seam power over enough independent realizations that
        // the ±20% band is a robust ≥6σ bound even at tiny xfade. A single small-xfade
        // seam averages only `xfade` frames of ±a noise (single-realization SD ≈
        // 0.50/√xfade ≈ 0.17 at xfade=8), so a per-draw ±20% band failed on correct
        // code — the flake this fixes (task #16). Averaging k `splitmix64`-seeded
        // realizations shrinks the SD ≈1/√k with no floor (measured — see
        // `ensemble_mean_variance_shrinks_with_k`); k = ⌈512/xfade⌉ gives a measured
        // ~7σ margin at the worst case (xfade=8). Tests the real property: the
        // crossfade preserves power in expectation for decorrelated content.
        let k = 512usize.div_ceil(xfade).max(1);
        let ratio = decorrelated_seam_power_ratio_mean(loop_frames, xfade, seed, k);
        prop_assert!(
            (0.80..=1.20).contains(&ratio),
            "decorrelated seam power ratio {ratio} out of band (xfade {xfade}, k {k})"
        );
    }
}

/// Regression for the estimator-variance flake fix (task #16): the exact
/// counterexample that tripped the old per-realization ±20% band —
/// `loop_frames=1091, xfade=11, seed=4320260872143147964`, whose single-draw ratio
/// is ≈1.259 — is comfortably in band once the ratio is estimated over enough
/// realizations. Proves the failure was small-`xfade` estimator variance (SD ≈
/// 0.50/√xfade ≈ 0.15 at xfade 11), NOT a power bug: the *expected* seam power is flat.
#[test]
fn decorrelated_seam_power_is_flat_at_the_reproduced_counterexample() {
    let (loop_frames, xfade_req, seed) = (1091usize, 11usize, 4_320_260_872_143_147_964u64);
    let xfade = xfade_req.min(loop_frames / 2).max(1);
    // The single draw that tripped the old per-realization band is genuinely a
    // high-variance ~1.26 outlier — a small-`xfade` estimate of a quantity whose
    // expectation is 1.0 — reproduced here with the exact failing seed.
    let single = decorrelated_seam_power_ratio(loop_frames, xfade, seed);
    assert!(
        single > 1.20,
        "the reproduced single-draw ratio is the >1.20 outlier the old band caught (got {single})"
    );
    // The expected seam power — averaged over enough realizations — is within ±20%.
    let k = 512usize.div_ceil(xfade).max(1);
    let mean = decorrelated_seam_power_ratio_mean(loop_frames, xfade, seed, k);
    assert!(
        (0.80..=1.20).contains(&mean),
        "expected decorrelated seam power is flat (mean {mean} over {k} realizations)"
    );
}

/// Establishes the fix's premise (addressing the #232 review): averaging over `k`
/// `splitmix64`-seeded realizations shrinks the seam-power estimator's SD ≈1/√k, so
/// the ±20% band is a ≥6σ bound at the worst-case small `xfade` — and it holds across
/// loop lengths (the estimator variance is a function of `xfade`, not length). A gross
/// loss of shrinkage (e.g. degenerate/identical sub-seeds) would leave `sd_ens ≈
/// sd_single` and fail the assertions. Deterministic (fixed base seeds).
#[test]
fn ensemble_mean_variance_shrinks_with_k() {
    let bases = 200u64;
    let stats = |vals: &[f64]| -> (f64, f64) {
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
        (mean, var.max(0.0).sqrt())
    };
    // Worst case is the smallest `xfade` (largest single-realization SD ≈ 0.50/√xfade);
    // two loop lengths confirm the margin does not depend on loop length. Base seeds
    // are `splitmix64`-mixed so the ~200-sample variance estimate itself is unbiased
    // (sequential bases would be as correlated as the sub-seeds this test guards).
    for &(loop_frames, xfade) in &[(191usize, 8usize), (523usize, 8usize)] {
        let k = 512usize.div_ceil(xfade).max(1); // = 64 → xfade·k = 512
        let single: Vec<f64> = (0..bases)
            .map(|b| decorrelated_seam_power_ratio(loop_frames, xfade, splitmix64(b ^ 0xABCD)))
            .collect();
        let ensemble: Vec<f64> = (0..bases)
            .map(|b| {
                decorrelated_seam_power_ratio_mean(loop_frames, xfade, splitmix64(b ^ 0xF00D), k)
            })
            .collect();
        let (_, sd_single) = stats(&single);
        let (mean_ens, sd_ens) = stats(&ensemble);
        let sigma_margin = 0.20 / sd_ens;
        println!(
            "loop={loop_frames} xfade={xfade} k={k}: sd_single={sd_single:.4} \
             sd_ens={sd_ens:.4} mean_ens={mean_ens:.4} => {sigma_margin:.1}sigma"
        );
        // Unbiased (estimates the true ratio 1.0); tolerance is >>the ~0.003 sampling SE.
        assert!(
            (mean_ens - 1.0).abs() < 0.04,
            "ensemble mean ≈ 1 (got {mean_ens})"
        );
        // ≈1/√k shrinkage (k=64 ⇒ √64=8): the ensemble SD falls well below the
        // single-realization SD. A degenerate/near-identical ensemble would not.
        assert!(
            sd_ens < sd_single * 0.30,
            "ensemble SD must shrink at loop={loop_frames} (single {sd_single:.4}, ens {sd_ens:.4})"
        );
        // The property's own k gives a measured ~7σ margin; require the ensemble here
        // to clear a comfortable ≥5σ (loose enough to never flake on the estimate).
        assert!(
            sigma_margin >= 5.0,
            "±20% must be ≥5σ at loop={loop_frames} (σ={sd_ens:.4}, margin {sigma_margin:.1})"
        );
    }
}
