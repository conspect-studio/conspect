//! EBU R128 / ITU-R BS.1770 loudness-normalisation tests (AUD-6).
//!
//! These verify the program-bus normaliser ([`multiview_audio::loudnorm`]) built
//! on the existing [`LoudnessMeter`](multiview_audio::loudness::LoudnessMeter):
//! a known-loudness signal driven through the processor must converge toward the
//! configured target LUFS within the live tolerance (±1 LU, brief §4.1) while the
//! emitted program bus never exceeds the −1.5 dBTP true-peak ceiling, the −70
//! LUFS gate excludes a silenced input, and discrete tracks are left byte-for-byte
//! identical (the ADR-R005 authenticity guarantee).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: synthetic-signal generation needs index<->float and float<->sample
// casts that are exact for the small ranges used here; test-only.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names
)]

use std::f64::consts::PI;
use std::time::Instant;

use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_audio::loudness::LoudnessMeter;
use multiview_audio::loudnorm::{
    LoudnessTarget, LoudnormProcessor, DEFAULT_TRUE_PEAK_CEILING_DBTP,
};
use proptest::prelude::*;

const FS: u32 = 48_000;
/// The live convergence tolerance from resilience-and-av §4.1 (±1 LU live, vs the
/// ±0.2 LU file-mode that single-pass live normalisation cannot match).
const LIVE_TOLERANCE_LU: f64 = 1.0;

/// One tick block of a `freq` Hz sine at peak amplitude `amp`, `frames` frames,
/// replicated across every channel of `layout`, phase-continuous via `phase`
/// (radians) carried across blocks so there is no click at a block boundary.
fn tone_block(
    format: AudioFormat,
    freq: f64,
    amp: f64,
    frames: usize,
    phase: &mut f64,
) -> AudioBlock {
    let ch = format.channel_count();
    let w = 2.0 * PI * freq / f64::from(format.sample_rate());
    let mut out = Vec::with_capacity(frames * ch);
    for _ in 0..frames {
        let s = (amp * phase.sin()) as f32;
        for _ in 0..ch {
            out.push(s);
        }
        *phase += w;
    }
    AudioBlock::from_interleaved(format, out).unwrap()
}

/// Measure the integrated loudness of a stream of already-processed blocks by
/// feeding them through a fresh meter (the canonical BS.1770 measurement chain).
fn integrated_of(format: AudioFormat, blocks: &[AudioBlock]) -> Option<f64> {
    let mut m = LoudnessMeter::new(format).unwrap();
    for b in blocks {
        m.push_interleaved(b.interleaved()).unwrap();
    }
    m.integrated()
}

/// Worst-case true-peak (dBTP) of a stream of processed blocks, via a fresh
/// meter's oversampled detector.
fn true_peak_of(format: AudioFormat, blocks: &[AudioBlock]) -> Option<f64> {
    let mut m = LoudnessMeter::new(format).unwrap();
    for b in blocks {
        m.push_interleaved(b.interleaved()).unwrap();
    }
    m.true_peak_dbtp()
}

/// A −23 dBFS 1 kHz tone reads ~−23 LUFS. Targeting −16 LUFS (web) the processor
/// must apply makeup gain that brings the emitted program bus up to within ±1 LU
/// of −16, and the result must never exceed the true-peak ceiling.
#[test]
fn program_bus_converges_to_target_lufs() {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    let target = LoudnessTarget::Streaming; // -16 LUFS
    let mut proc = LoudnormProcessor::new(format, target).unwrap();

    // A quiet source: a -23 dBFS sine, ~7 dB under the -16 LUFS web target.
    let amp = 10f64.powf(-23.0 / 20.0);
    let frames = 1920; // one 25 fps tick @ 48 kHz
    let mut phase = 0.0;

    // Run ~8 s so the integrated loudness and the gain smoother both settle.
    let ticks = 25 * 8;
    let mut out = Vec::with_capacity(ticks);
    for _ in 0..ticks {
        let block = tone_block(format, 1000.0, amp, frames, &mut phase);
        out.push(proc.process(block));
    }

    // Measure only the settled tail (skip the smoother's ramp-in).
    let tail = &out[out.len() / 2..];
    let measured = integrated_of(format, tail).expect("settled tail has loudness");
    assert!(
        (measured - target.lufs()).abs() <= LIVE_TOLERANCE_LU,
        "normalised program bus integrated loudness {measured} LUFS must be within \
         ±{LIVE_TOLERANCE_LU} LU of the {} LUFS target",
        target.lufs()
    );

    // Whole-run true-peak never exceeds the ceiling.
    let tp = true_peak_of(format, &out).expect("a loud tone has a true peak");
    assert!(
        tp <= DEFAULT_TRUE_PEAK_CEILING_DBTP + 0.1,
        "true peak {tp} dBTP must not exceed the {DEFAULT_TRUE_PEAK_CEILING_DBTP} dBTP ceiling"
    );
}

/// The −1.5 dBTP true-peak ceiling is hard: a source already AT 0 dBFS that the
/// target would push LOUDER must be limited so the emitted true-peak stays at or
/// below the ceiling — normalisation never clips.
#[test]
fn true_peak_ceiling_is_never_exceeded() {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    // A near-0 dBFS source measuring ~0 LUFS; the -16 LUFS target wants to make it
    // QUIETER, but flip to broadcast -23 with a HOT source to exercise the limiter
    // on the *makeup-up* path: use a quiet-but-peaky source.
    let target = LoudnessTarget::Streaming;
    let mut proc = LoudnormProcessor::new(format, target).unwrap();

    // A loud full-scale 0 dBFS tone (reads ~0 LUFS): target -16 means a NEGATIVE
    // gain, so it cannot clip — but a true-peak-pushing source with low loudness
    // can. Build a -10 LUFS-ish tone that already sits at 0 dBFS sample peak by
    // using a low duty: here, a 0 dBFS sine (≈0 LUFS) is louder than target, the
    // gain is negative, peak drops. To exercise the *ceiling*, target Broadcast
    // (-23) is even quieter. So instead force the up-path: a quiet -30 dBFS sine
    // whose makeup gain (+14 LU) would push a co-incident full-scale transient
    // over 0 dBTP unless the limiter caps it.
    let frames = 1920;
    let mut phase = 0.0;
    let quiet = 10f64.powf(-30.0 / 20.0);
    let mut out = Vec::new();
    for tick in 0..(25 * 8) {
        // Mostly a quiet -30 dBFS tone, but inject a full-scale (1.0) burst every
        // so often so a naive +14 LU makeup gain would slam it to +14 dBTP.
        let mut block = tone_block(format, 1000.0, quiet, frames, &mut phase);
        if tick % 10 == 0 {
            let ch = format.channel_count();
            let mut s = block.interleaved().to_vec();
            for v in s.iter_mut().take(ch * 64) {
                *v = 1.0;
            }
            block = AudioBlock::from_interleaved(format, s).unwrap();
        }
        out.push(proc.process(block));
    }

    let tp = true_peak_of(format, &out).expect("a loud burst has a true peak");
    assert!(
        tp <= DEFAULT_TRUE_PEAK_CEILING_DBTP + 0.1,
        "even with a +makeup-gain quiet source plus full-scale bursts the emitted \
         true peak {tp} dBTP must stay at/below the {DEFAULT_TRUE_PEAK_CEILING_DBTP} dBTP ceiling"
    );
}

/// Pure silence is below the −70 LUFS absolute gate, so the processor applies NO
/// makeup gain (it must not try to amplify a silenced input toward the target —
/// brief §4.1: the gate "correctly excludes silence from a lost input"). The
/// emitted block stays silent.
#[test]
fn silence_below_gate_is_not_amplified() {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    let mut proc = LoudnormProcessor::new(format, LoudnessTarget::Streaming).unwrap();
    let frames = 1920;
    for _ in 0..(25 * 4) {
        let silent = AudioBlock::silence(format, frames);
        let out = proc.process(silent);
        for &s in out.interleaved() {
            assert_eq!(s, 0.0, "silence must remain silence (no gate-driven gain)");
        }
    }
    // The smoothed gain must not have run away toward the +inf the (-70 - (-16))
    // makeup would imply if silence were ungated.
    assert!(
        proc.current_gain_db() <= 0.5,
        "gated silence must not drive the makeup gain up (got {} dB)",
        proc.current_gain_db()
    );
}

/// A discrete track is the ADR-R005 authenticity guarantee: the normaliser is for
/// the program bus ONLY. Feeding the same block through the discrete path must
/// return it byte-for-byte identical (no gain, no limiter).
#[test]
fn discrete_tracks_unaltered() {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    let mut proc = LoudnormProcessor::new(format, LoudnessTarget::Streaming).unwrap();
    let amp = 10f64.powf(-23.0 / 20.0);
    let mut phase = 0.0;
    let block = tone_block(format, 1000.0, amp, 1920, &mut phase);

    // The processor exposes a discrete passthrough that must be the identity.
    let discrete = LoudnormProcessor::discrete_passthrough(&block);
    assert_eq!(
        discrete.interleaved(),
        block.interleaved(),
        "discrete tracks must be byte-identical to the input (unaltered)"
    );

    // Even after the processor has been driving the PROGRAM path (and built up a
    // makeup gain), the discrete passthrough is still the identity — it shares no
    // state with the program normaliser.
    let _ = proc.process(block.clone());
    let discrete2 = LoudnormProcessor::discrete_passthrough(&block);
    assert_eq!(discrete2.interleaved(), block.interleaved());
}

/// The two named targets are the documented broadcast/streaming LUFS values.
#[test]
fn targets_are_documented_lufs() {
    approx::assert_abs_diff_eq!(LoudnessTarget::Broadcast.lufs(), -23.0, epsilon = 1e-9);
    approx::assert_abs_diff_eq!(LoudnessTarget::Streaming.lufs(), -16.0, epsilon = 1e-9);
    approx::assert_abs_diff_eq!(LoudnessTarget::Custom(-20.0).lufs(), -20.0, epsilon = 1e-9);
    // The default ceiling is the brief's -1.5 dBTP.
    approx::assert_abs_diff_eq!(DEFAULT_TRUE_PEAK_CEILING_DBTP, -1.5, epsilon = 1e-9);
}

/// Processing preserves the block shape exactly (same frame count, same format):
/// the normaliser is sample-for-sample on the program bus, never resizing it
/// (invariant #1: the program-bus tick stays exactly its budget length).
#[test]
fn process_preserves_block_shape() {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    let mut proc = LoudnormProcessor::new(format, LoudnessTarget::Broadcast).unwrap();
    let amp = 10f64.powf(-30.0 / 20.0);
    let mut phase = 0.0;
    for frames in [1601usize, 1602, 1920] {
        let block = tone_block(format, 1000.0, amp, frames, &mut phase);
        let out = proc.process(block);
        assert_eq!(out.frame_count(), frames);
        assert_eq!(out.format(), format);
    }
}

/// RT-8b perf guard: [`LoudnormProcessor::process`] must be genuinely `O(block)`
/// cheap — not a multiple-of-the-meter cost. Under a `DropOnOverload` shed the bake
/// consumer catches the program bus up across the whole dropped span in a SINGLE
/// `tick_to`, so one `process` call sees a block of hundreds of thousands of frames;
/// if `process` runs the 4× oversampled true-peak FIR over every sample of such a
/// block it stalls the consumer for seconds and audio falls behind the output-tick
/// timeline (the rt8b lip-sync failure).
///
/// For program audio sitting near its target loudness — the overwhelmingly common
/// case — the gained sample peak is comfortably below the true-peak ceiling, so the
/// limiter's sample-peak pre-screen proves the block safe and skips the oversampling
/// FIR entirely (it only primes the seam window). `process` then costs essentially
/// just the one K-weighting loudness pass plus a couple of cheap linear scans — well
/// UNDER the cost of one full metering pass (which additionally runs the true-peak
/// FIR). The regression — running the FIR over the whole block (the meter's own
/// true-peak pass plus the limiter's probe + persistent passes) — costs roughly
/// THREE metering passes.
///
/// Self-calibrating bound (so it is not flaky across machines/build profiles): one
/// full `LoudnessMeter::push_interleaved` over the same samples — K-weighting PLUS
/// the meter's true-peak FIR — is the reference "one metering pass". A below-ceiling
/// `process` must come in well under that (the FIR is skipped); the `0.6×` ceiling
/// both forbids the original ~3× regression AND locks in that the oversampling FIR
/// is not run over a block the sample peak already proved safe.
#[test]
fn process_is_o_block_cheap_on_a_large_catch_up_block() {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    // 400-tick catch-up @ 1920 frames/tick: the rt8b shed span in one block. A
    // quiet -23 dBFS tone (well below the ceiling) so the limiter's sample-peak
    // pre-screen engages — the common program-audio case the consumer must absorb.
    let frames = 400 * 1920;
    let amp = 10f64.powf(-23.0 / 20.0);
    let mut phase = 0.0;
    let block = tone_block(format, 1000.0, amp, frames, &mut phase);

    // Reference: one full metering pass (K-weight + true-peak FIR) over the block.
    let mut meter = LoudnessMeter::new(format).unwrap();
    let t = Instant::now();
    meter.push_interleaved(block.interleaved()).unwrap();
    let one_meter_pass = t.elapsed();

    // The processor over the SAME block.
    let mut proc = LoudnormProcessor::new(format, LoudnessTarget::Streaming).unwrap();
    let t = Instant::now();
    let out = proc.process(block);
    let process_cost = t.elapsed();
    assert_eq!(out.frame_count(), frames, "block shape must be preserved");

    let ratio = process_cost.as_secs_f64() / one_meter_pass.as_secs_f64().max(1e-9);
    assert!(
        ratio <= 0.6,
        "RT-8b perf: process() of a below-ceiling {frames}-frame catch-up block took \
         {process_cost:?}, {ratio:.2}x one metering pass ({one_meter_pass:?}); it must stay \
         O(block) cheap (the sample-peak pre-screen must skip the oversampling true-peak FIR) \
         so the bake consumer never stalls under a shed"
    );
}

/// Run a single-amplitude tone through a processor for long enough to settle and
/// return the steady makeup gain (dB) it ends on.
fn settled_gain_db(amp: f64) -> f64 {
    let format = AudioFormat::new(FS, ChannelLayout::Stereo);
    let mut proc = LoudnormProcessor::new(format, LoudnessTarget::Streaming).unwrap();
    let mut phase = 0.0;
    for _ in 0..(25 * 8) {
        let block = tone_block(format, 1000.0, amp, 1920, &mut phase);
        let _ = proc.process(block);
    }
    proc.current_gain_db()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Monotonic gain behaviour: a QUIETER source (further below the target) must
    /// receive at least as much steady-state makeup gain as a louder source. The
    /// normaliser pushes both toward the same target, so the quieter one needs
    /// MORE gain — never less. (Allow a tiny epsilon for the true-peak limiter and
    /// gate edges; the relationship is the monotone one this asserts.)
    #[test]
    fn quieter_source_gets_no_less_makeup_gain(
        loud_dbfs in -25.0f64..=-12.0,
        delta_db in 3.0f64..=20.0,
    ) {
        let loud_amp = 10f64.powf(loud_dbfs / 20.0);
        let quiet_amp = 10f64.powf((loud_dbfs - delta_db) / 20.0);
        let g_loud = settled_gain_db(loud_amp);
        let g_quiet = settled_gain_db(quiet_amp);
        prop_assert!(
            g_quiet >= g_loud - 0.5,
            "quieter source ({}-dBFS, gain {g_quiet} dB) must get >= the louder \
             source ({loud_dbfs}-dBFS, gain {g_loud} dB) makeup gain",
            loud_dbfs - delta_db
        );
    }
}
