# Audio placement across overlapping timestamp epochs

The audio decode queue can contain distinct epochs whose raw timestamps overlap after a backwards splice. `EpochLog::epoch_of` uses the newest matching epoch. A video anchor can therefore map a later queued epoch while the older queue head has no mapping. Holding that head forever prevents valid anchored PCM behind it from playing.

The placer discards an unmapped head only when a strictly later queued epoch has a video mapping. It does not infer an anchor, unlock audio, or discard an already mapped older block. Without a video-mapped successor, the existing bounded hold policy remains. Discards count as skipped frames and reduce queue occupancy. The successor scan is cached once per placement pass; the pass remains linear in the bounded queue, with no additional heap allocation, blocking operation, or output-clock dependency.

This policy inherits the existing raw-clock identity limitation: raw timestamps alone cannot distinguish two overlapping epochs. If video subsequently advances beyond the replay span into a range that uniquely identifies the old epoch, previously discarded old samples cannot be recovered. The policy trades that bounded recovery opportunity for progress of currently video-mapped PCM; it does not establish stronger cross-rail epoch identity. A future explicit shared epoch identity must be designed across both rails, not inferred from wall-clock timing.

Regression coverage checks replay progress, preservation of mapped older audio, no discard without a video anchor, and frame accounting. The original CI silence failure is a separate causal claim: a local integration-test pass after this repair does not by itself prove that intermittent failure's root cause.

The production path is `multiview-cli::audio` → `audio_decode_loop_anchored` → `anchored_publish_loop` with `require_video_anchor = true`. Pure queue tests exercise the same placer policy without libav or timing-dependent input.
