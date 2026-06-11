# ADR-T006: Long-run clock drift: monotonic master + PI/dead-band loop + adaptive audio resampling

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Master = system CLOCK_MONOTONIC driving the output pacer. Per source, run a control loop: EMA-filtered buffer-level/PTS error (GStreamer 31/32), dead-band ~40 ms, multi-sample accumulation, PI correction. Video: whole-frame select/drop/duplicate at the compositor. Audio: continuous SOFT resampling by measured ppm - swresample with async>1 (NOT async=1) or explicit min_comp/comp_duration/max_soft_comp driven by swr_set_compensation+swr_next_pts from the master-clock A/V offset, or soxr SOXR_VR (soxr_set_io_ratio). Reserve hard fill/trim as a discontinuity-only safety net. Unify all audio to 48k fltp before mixing.

## Rationale

Independent crystals drift tens-hundreds of ppm; uncorrected -> overrun/underrun + desync over hours. Verification corrected that async=1 enables ONLY fill/trim (hard); soft stretch needs async>1.0001 (max_soft_comp set only then). min_hard_comp default 0.1 s lets ~100 ms accumulate - larger than the EBU R37 window - so correction must be driven from the master offset, not swr defaults. Frame-select video + resample audio keyed to the SAME per-source drift estimate is the NDI Framesync model.

## Alternatives considered

async=1 alone (no stretch, glitchy); making an input/audio-HW clock the master (one hiccup stalls the multiview); ignoring drift (eventual desync/buffer blowup); audio-only resampling (video buffers still overrun/underrun).

## Consequences

Requires soak testing for hours/days (acceptance: max |A/V offset| within window + zero output gaps over >=72 h). Per-source high-quality resampling (soxr VR/rubato) has CPU cost at scale - may need a quality/throughput tradeoff.
