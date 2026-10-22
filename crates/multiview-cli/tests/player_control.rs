//! RED tests for the **single-authority transport coupling** between a media
//! player's video rail and its audio rail (ADR-T019 Defect-1 fix).
//!
//! The defect the cross-vendor review caught: both rails drained the SAME
//! `TransportMailbox`, whose `drain()` is a destructive `mem::take`, so whichever
//! rail drained a verb first **consumed** it and the other rail **missed** it —
//! `ArmExit`/`Stop`/`Vamp` reached only one rail, so the rails wrapped/exited at
//! different times, defeating the same-boundary guarantee.
//!
//! The fix: the **video** `stream_player` is the SOLE mailbox consumer and
//! PUBLISHES its authoritative transport decisions to a wait-free
//! [`PlayerControlBus`] that the audio rail SAMPLES and FOLLOWS (audio samples,
//! never independently consumes verbs — invariant #1). These tests pin that
//! contract at the unit level (feature-independent — no libav/GPU).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Arc;

use multiview_audio::format::{AudioFormat, ChannelLayout};
use multiview_audio::loopdeck::LoopDeck;
use multiview_cli::player::{AudioTransport, PlayerControlBus};
use multiview_core::time::MediaTime;

const RATE: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(RATE, ChannelLayout::Stereo)
}

/// A deck over a constant non-silent body (so "is it playing vs silent" is crisp).
fn deck(loop_frames: usize, xfade: usize) -> LoopDeck {
    let decoded = vec![0.3f32; (loop_frames + xfade) * 2];
    LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).expect("valid deck")
}

fn block_energy(block: &multiview_audio::AudioBlock) -> f64 {
    block
        .interleaved()
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum()
}

// ----------------------------------------------------------------------------
// 1. The control bus carries every transport decision wait-free, latest-wins
//    with a monotonic generation — so a late-sampling reader still sees it.
// ----------------------------------------------------------------------------

#[test]
fn the_control_bus_publishes_transport_state_for_the_audio_rail_to_sample() {
    let bus = PlayerControlBus::new();
    // Initially: the video has not published — a fresh reader sees the default
    // (vamping, no exit) so a boot-vamping player loops from the first block.
    let initial = bus.load();
    assert_eq!(initial.state, AudioTransport::Vamping);
    assert!(initial.exit_arm_anchor.is_none());

    // The video drains `Pause` and publishes it.
    let g0 = initial.generation;
    bus.publish(AudioTransport::Paused, None);
    let after = bus.load();
    assert_eq!(after.state, AudioTransport::Paused);
    assert!(
        after.generation > g0,
        "each publish bumps the monotonic generation"
    );

    // The video drains `Stop`, then `Vamp` — latest-wins.
    bus.publish(AudioTransport::Stopped, None);
    bus.publish(AudioTransport::Vamping, None);
    let latest = bus.load();
    assert_eq!(latest.state, AudioTransport::Vamping, "latest publish wins");
}

// ----------------------------------------------------------------------------
// 2. The audio rail FOLLOWS the published state — vamp/pause/stop mirror it.
// ----------------------------------------------------------------------------

#[test]
fn the_audio_rail_follows_the_published_pause_and_resume() {
    let bus = PlayerControlBus::new();
    let mut deck = deck(400, 40);
    let mut last_gen = 0u64;

    // Block 0: vamping (the default) → audio plays the tone.
    apply_control(&bus, &mut deck, &mut last_gen);
    let b0 = deck.read(200);
    assert!(block_energy(&b0) > 1.0, "vamping audio plays the tone");

    // The video drains Pause and publishes; the audio samples it next block.
    bus.publish(AudioTransport::Paused, None);
    apply_control(&bus, &mut deck, &mut last_gen);
    let b1 = deck.read(200);
    assert_eq!(
        block_energy(&b1),
        0.0,
        "paused audio (following the video) is silent"
    );

    // Resume.
    bus.publish(AudioTransport::Vamping, None);
    apply_control(&bus, &mut deck, &mut last_gen);
    let b2 = deck.read(200);
    assert!(
        block_energy(&b2) > 1.0,
        "resumed audio plays the tone again"
    );
}

// ----------------------------------------------------------------------------
// 3. THE CORE DEFECT: ONE ArmExit must reach BOTH rails and fire at the SAME
//    wrap boundary. The video publishes the arm with its media-time anchor; the
//    audio arms at the SAME boundary by construction (shared geometry, same anchor).
// ----------------------------------------------------------------------------

#[test]
fn one_arm_exit_fires_audio_at_the_same_wrap_boundary_as_the_video() {
    // L = 1 s at 48 kHz. The video arms its exit at media-time 1.5 s (mid lap 1),
    // so BOTH rails must fire at the next vamp boundary = 2.0 s = sample 96_000.
    let loop_frames = 48_000usize;
    let xfade = 480usize;
    let mut deck = deck(loop_frames, xfade);
    let bus = PlayerControlBus::new();
    let mut last_gen = 0u64;

    // The video drains ArmExit at output media-time 1.5 s and publishes the anchor.
    let arm_anchor = MediaTime::from_nanos(1_500_000_000); // 1.5 s
    bus.publish(AudioTransport::Vamping, Some(arm_anchor));
    apply_control(&bus, &mut deck, &mut last_gen);

    // Read across the 2.0 s boundary. Before 2.0 s (sample 96_000) audio plays;
    // after the exit fade tail it is SILENT. The exit must fire at THE SAME 2.0 s
    // boundary the video uses — not at the audio's own cursor position.
    // Read up to just before the boundary: still playing.
    let before = deck.read_at(95_000, 480);
    assert!(
        block_energy(&before) > 1.0,
        "before the shared 2.0 s boundary, audio still plays"
    );
    // Read well past the boundary + the fade tail: silent.
    let after = deck.read_at(96_000 + xfade as u64 + 1_000, 480);
    assert_eq!(
        block_energy(&after),
        0.0,
        "the armed exit must fire at the SAME 2.0 s wrap boundary as the video (sample 96_000), then go silent"
    );
}

/// The audio rail's per-block control step: sample the bus, and if the generation
/// advanced, apply the published state to the deck (this is what `player_audio_loop`
/// does each block instead of draining the mailbox).
fn apply_control(bus: &PlayerControlBus, deck: &mut LoopDeck, last_gen: &mut u64) {
    let ctrl = bus.load();
    if ctrl.generation == *last_gen {
        return;
    }
    *last_gen = ctrl.generation;
    match ctrl.state {
        AudioTransport::Vamping => {
            deck.vamp();
            if let Some(anchor) = ctrl.exit_arm_anchor {
                // Arm at the video's anchor so both fire at the SAME boundary.
                let anchor_frame = media_time_to_frame(anchor);
                deck.arm_exit_at(anchor_frame);
            }
        }
        AudioTransport::Stopped => deck.stop(),
        // `Paused` and any future (`#[non_exhaustive]`) variant ride silence.
        _ => deck.pause(),
    }
}

/// Convert an output media-time to the audio rail's absolute 48 kHz frame index.
fn media_time_to_frame(t: MediaTime) -> u64 {
    let ns = t.as_nanos().max(0);
    u64::try_from((i128::from(ns) * 48_000) / 1_000_000_000).unwrap_or(0)
}

/// A bus shared by `Arc` between the video and audio threads is wait-free to
/// load — sampled every audio block off the hot path.
#[test]
fn the_bus_is_arc_shareable_across_threads() {
    let bus = Arc::new(PlayerControlBus::new());
    let writer = Arc::clone(&bus);
    let handle = std::thread::spawn(move || {
        writer.publish(AudioTransport::Stopped, None);
    });
    handle.join().unwrap();
    assert_eq!(bus.load().state, AudioTransport::Stopped);
}
