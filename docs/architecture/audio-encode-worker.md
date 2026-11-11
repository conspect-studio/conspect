# Program audio encode off the bake consumer (design note)

## Problem

The bake consumer is the single thread that, per output tick, bakes the overlay canvas,
offers it to the video rendition workers, and then — inline — mixes program audio, runs
loudness normalisation and meters, **AAC-encodes the program bus and every selected discrete
track**, computes watermarks and dispatches the packets.

On the production NVIDIA host (1080p29.97, budget 33.4 ms) with six cells, auto captions and
four discrete audio tracks on two outputs, an instrumented build measured, per slow iteration:
overlay bake p50 22 ms (p90 32 ms), program AAC encode p50 7.5 ms, discrete-track encode p50
12 ms (p90 19 ms). About 300 iterations per 15 minutes exceeded 40 ms. When eight or more
consecutive ticks were lost, the frame-substitution guard (`FRAME_SUBSTITUTION_LIMIT`) failed
the run with "output fell N ticks behind the clock" and the service restarted — four times
on the first day the track configuration ran. The video encoder was not the bottleneck.

## Decision

Move the **AAC encode and the audio packet dispatch** to a dedicated `multiview-audio-encode`
worker thread, the same shape as the existing video rendition workers:

- The consumer keeps everything that must stay on the tick (mixing the bus, loudness
  normalisation, meters, display/AES67/WHEP taps — all cheap). It then hands an owned
  per-tick job — the post-loudnorm program block plus a clone of each selected discrete
  track's block (or `None` for silence) — to the worker over a bounded channel.
- The worker owns the `AudioRenditionEncoder`, encodes the program and the tracks in tick
  order, computes the watermarks, sends `DispatchEvent::Audio` to the dispatcher and taps
  the preview. At end of run it encodes the tail and sends `FinishAudio`.
- Sample-accurate A/V lock is unchanged: each job carries exactly the tick's sample budget, the
  encoder's sample counter (PTS) advances only through jobs, and jobs are processed strictly in
  order, so audio PTS is still a pure function of the tick counter (invariant #3).
- Bounded queue (`AUDIO_WORKER_QUEUE_TICKS`, 8). The consumer offers with the same
  cadence-derived horizon as the video workers: a transient encoder stall back-pressures the
  consumer only within that horizon; a worker that cannot drain within it fails the run
  explicitly — exactly the existing video-worker contract, never an unbounded wait
  (invariants #1/#10).
- Drop is not an option for audio (a missing block would desynchronise PTS), so the
  bounded-wait-then-fail contract is the correct one, as for video.

## Consequences

- The consumer's per-tick audio cost drops from ~20 ms to the mix/meter cost (< 1 ms
  measured), returning the headroom the overlay bake needs.
- Audio encode now runs in parallel with the next tick's bake (one more core in use).
- One more copy per tick: the program block (~6 KB at 48 kHz stereo) and each track block
  are cloned into the job. Negligible next to the encode cost.
- The overlay bake itself (p90 32 ms in this configuration) remains the next structural cost;
  it is a separate change (GPU overlay sub-pass), not addressed here.

## Evidence required

- Unit: jobs encode strictly in tick order with a single flush; a wedged worker fails the offer
  within the horizon (no unbounded wait); an encoder error surfaces at join; joining while the
  consumer drains a bounded event channel completes (the EOF shape). Packet/PTS equivalence with
  the former inline path is proven end to end (identical encoder inputs and order; the
  `discrete_audio_tracks` suite decodes every rendition), not by a unit fake.
- Pipeline: an existing A/V lock test (tracks + program) passes unchanged.
- Hardware: on the production host, the instrumented consumer's audio stage drops to < 1 ms and
  there are no "fell behind" exits over a soak.
