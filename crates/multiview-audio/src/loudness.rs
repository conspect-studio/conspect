//! EBU R128 / ITU-R BS.1770 loudness metering (pure DSP).
//!
//! [`LoudnessMeter`] implements the full BS.1770-4 measurement chain:
//!
//! 1. **K-weighting** ([`crate::filter`]) per channel — a high-shelf pre-filter
//!    cascaded with an RLB high-pass.
//! 2. **Mean square** of the K-weighted signal, accumulated in 100 ms
//!    sub-blocks.
//! 3. **Channel weighting** — front/centre at unity, surround at +1.5 dB
//!    (factor 1.41), LFE excluded.
//! 4. **Gating** — 400 ms gating blocks with 75 % overlap, an absolute
//!    `-70 LUFS` gate, then a relative gate `10 LU` below the absolute-gated
//!    mean.
//! 5. **Momentary** (400 ms), **short-term** (3 s), **integrated** (gated) and
//!    **loudness range (LRA)** per EBU Tech 3342.
//!
//! True-peak (4× oversampled, dBTP) is provided alongside via
//! [`crate::truepeak`].
//!
//! The math here is deliberately allocation-light and lives off the hot path
//! (ADR-R006): the engine taps audio into a meter on a separate thread and
//! never blocks on it.
use crate::error::{AudioError, Result};
use crate::filter::KWeightFilter;
use crate::format::{AudioFormat, ChannelLayout};
use crate::truepeak::TruePeakDetector;

#[doc(no_inline)]
pub use crate::filter::k_weight_coeffs;

/// The BS.1770 absolute gating threshold, in LUFS.
pub const ABSOLUTE_GATE_LUFS: f64 = -70.0;
/// The BS.1770 relative gating offset (relative gate = gated mean + this), in LU.
pub const RELATIVE_GATE_OFFSET_LU: f64 = -10.0;
/// The EBU Tech 3342 relative gating offset for LRA, in LU.
pub const LRA_RELATIVE_GATE_OFFSET_LU: f64 = -20.0;
/// The BS.1770 loudness offset constant, in LU.
pub const LOUDNESS_OFFSET: f64 = -0.691;

/// Length of a gating sub-block, in milliseconds. Four of these make a 400 ms
/// gating block (75 % overlap between successive blocks).
const SUBBLOCK_MS: u64 = 100;
/// Sub-blocks per 400 ms gating block / per momentary window.
const SUBBLOCKS_PER_BLOCK: usize = 4;
/// Sub-blocks per 3 s short-term window.
const SUBBLOCKS_PER_SHORT_TERM: usize = 30;

/// The per-channel weighting `G` from ITU-R BS.1770: front L/R and centre at
/// unity, the two surround channels at +1.5 dB (`10^(1.5/20) ≈ 1.41`), and the
/// LFE excluded (`0`). Out-of-range indices weight to zero.
#[must_use]
pub fn channel_weight(layout: ChannelLayout, channel: usize) -> f64 {
    // 10^(1.5/20) rounded to the BS.1770-quoted 1.41.
    const SURROUND: f64 = 1.41;
    match layout {
        ChannelLayout::Mono => {
            if channel == 0 {
                1.0
            } else {
                0.0
            }
        }
        ChannelLayout::Stereo => {
            if channel < 2 {
                1.0
            } else {
                0.0
            }
        }
        // Order: L(0) R(1) C(2) LFE(3) Ls(4) Rs(5).
        ChannelLayout::FivePointOne => match channel {
            0..=2 => 1.0,
            4 | 5 => SURROUND,
            // LFE (3) and any out-of-range channel are excluded.
            _ => 0.0,
        },
    }
}

/// Convert a channel-summed weighted mean-square into a loudness in LUFS.
#[must_use]
fn energy_to_lufs(weighted_mean_square: f64) -> f64 {
    LOUDNESS_OFFSET + 10.0 * weighted_mean_square.log10()
}

/// Exact `usize -> f64` for sample/sub-block counts. Audio lengths are far
/// below `2^53`, so the conversion is lossless; the cast is the only way to go
/// from `usize` to `f64` and cannot fail in this range.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)] // reason: count fits exactly in f64 (< 2^53); no fallible From exists.
#[must_use]
const fn count_f64(n: usize) -> f64 {
    n as f64
}

/// A streaming EBU R128 / BS.1770 loudness meter for one audio stream.
#[derive(Debug)]
pub struct LoudnessMeter {
    format: AudioFormat,
    filters: Vec<KWeightFilter>,
    weights: Vec<f64>,
    true_peak: Vec<TruePeakDetector>,

    /// Samples per 100 ms sub-block (per channel).
    subblock_len: usize,
    /// Samples accumulated into the current sub-block (per channel).
    subblock_filled: usize,
    /// Running sum over the current sub-block of `Σ_c G_c · y_c²`.
    subblock_weighted_sq: f64,

    /// Per-sub-block weighted mean square `(Σ_c G_c · y_c²) / subblock_len`.
    /// One entry per completed 100 ms sub-block.
    subblocks: Vec<f64>,

    /// Whether [`push_interleaved`](Self::push_interleaved) advances the 4×
    /// oversampled per-channel true-peak detectors. `true` by default (the meter
    /// reports dBTP via [`true_peak_dbtp`](Self::true_peak_dbtp)). A loudness-only
    /// consumer that runs its **own** true-peak detector — the program-bus
    /// normaliser's limiter (AUD-6) — disables this with
    /// [`without_true_peak`](Self::without_true_peak) so the expensive FIR is not
    /// evaluated twice over every sample.
    track_true_peak: bool,
}

impl LoudnessMeter {
    /// Construct a meter for the given format.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`] if the sample rate or channel count
    /// is zero.
    pub fn new(format: AudioFormat) -> Result<Self> {
        if !format.is_valid() {
            return Err(AudioError::InvalidFormat(
                "sample rate and channels must be non-zero",
            ));
        }
        let channels = format.channel_count();
        let layout = format.channel_layout();
        let filters = (0..channels)
            .map(|_| KWeightFilter::new(format.sample_rate()))
            .collect();
        let weights = (0..channels).map(|c| channel_weight(layout, c)).collect();
        let true_peak = (0..channels).map(|_| TruePeakDetector::new()).collect();

        // subblock_len = round(sample_rate * 100ms / 1000). Use u128 to avoid
        // overflow on the multiply, then a checked narrowing.
        let len_u128 = (u128::from(format.sample_rate()) * u128::from(SUBBLOCK_MS) + 500) / 1000;
        let subblock_len = usize::try_from(len_u128).unwrap_or(usize::MAX).max(1);

        Ok(Self {
            format,
            filters,
            weights,
            true_peak,
            subblock_len,
            subblock_filled: 0,
            subblock_weighted_sq: 0.0,
            subblocks: Vec::new(),
            track_true_peak: true,
        })
    }

    /// Disable the oversampled true-peak FIR in [`push_interleaved`](Self::push_interleaved).
    ///
    /// A loudness-only consumer that maintains its **own** true-peak detector — the
    /// program-bus loudness normaliser, whose feedforward limiter already runs a 4×
    /// oversampled true-peak FIR over the gained output (AUD-6) — does not need the
    /// meter to run the same FIR a second time over the raw input. Disabling it
    /// makes per-block metering one K-weighting pass rather than K-weighting plus a
    /// redundant true-peak pass, so a large `DropOnOverload` catch-up block cannot
    /// stall the bake consumer (RT-8b). With true-peak disabled
    /// [`true_peak_dbtp`](Self::true_peak_dbtp) stays `None` (no peak is tracked).
    #[must_use]
    pub const fn without_true_peak(mut self) -> Self {
        self.track_true_peak = false;
        self
    }

    /// The meter's audio format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Feed interleaved PCM samples (frame-major) into the meter.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`] if the sample count is not a whole
    /// number of frames for the format.
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Result<()> {
        let channels = self.format.channel_count();
        if channels == 0 || samples.len() % channels != 0 {
            return Err(AudioError::RaggedBlock {
                samples: samples.len(),
                channels,
            });
        }
        for frame in samples.chunks_exact(channels) {
            let mut weighted_sq = 0.0;
            for (c, &raw) in frame.iter().enumerate() {
                let x = f64::from(raw);
                // True-peak runs on the raw (pre-K-weight) sample, unless a
                // loudness-only consumer disabled it (it runs its own — RT-8b).
                if self.track_true_peak {
                    if let Some(tp) = self.true_peak.get_mut(c) {
                        tp.push(x);
                    }
                }
                let (Some(filter), Some(&g)) = (self.filters.get_mut(c), self.weights.get(c))
                else {
                    continue;
                };
                let y = filter.process(x);
                weighted_sq += g * y * y;
            }
            self.subblock_weighted_sq += weighted_sq;
            self.subblock_filled += 1;
            if self.subblock_filled >= self.subblock_len {
                let mean_sq = self.subblock_weighted_sq / count_f64(self.subblock_len);
                self.subblocks.push(mean_sq);
                self.subblock_weighted_sq = 0.0;
                self.subblock_filled = 0;
            }
        }
        Ok(())
    }

    /// Bound the retained sub-block history to the most recent `keep` sub-blocks,
    /// dropping older ones from the front.
    ///
    /// The streaming meter otherwise accumulates one sub-block per 100 ms for the
    /// whole run; a **live** consumer (e.g. the program-bus normaliser, AUD-6)
    /// that only needs the short-term/momentary windows calls this each tick so
    /// memory stays bounded regardless of run length. The momentary (400 ms) and
    /// short-term (3 s) windows read the tail and so are preserved as long as
    /// `keep` covers at least their span; the integrated/LRA computations become
    /// windowed over the retained history rather than the whole run — the live
    /// trade documented in resilience-and-av §4.1. A no-op when fewer than `keep`
    /// sub-blocks exist; `keep == 0` is treated as `1` so at least the in-progress
    /// window survives.
    pub fn retain_recent(&mut self, keep: usize) {
        let keep = keep.max(1);
        if self.subblocks.len() > keep {
            let drop = self.subblocks.len() - keep;
            self.subblocks.drain(0..drop);
        }
    }

    /// Mean of the most recent `n` sub-block weighted mean-squares (the energy
    /// of an `n × 100 ms` sliding window), or `None` if fewer than `n`
    /// sub-blocks exist.
    fn window_energy(&self, n: usize) -> Option<f64> {
        if n == 0 || self.subblocks.len() < n {
            return None;
        }
        let start = self.subblocks.len() - n;
        let slice = self.subblocks.get(start..)?;
        let sum: f64 = slice.iter().sum();
        Some(sum / count_f64(n))
    }

    /// Momentary loudness (most recent 400 ms), in LUFS, or `None` if the window
    /// energy is below the absolute gate (e.g. silence) or not enough audio has
    /// been seen.
    #[must_use]
    pub fn momentary(&self) -> Option<f64> {
        self.window_loudness(SUBBLOCKS_PER_BLOCK)
    }

    /// Short-term loudness (most recent 3 s), in LUFS, or `None`.
    #[must_use]
    pub fn short_term(&self) -> Option<f64> {
        self.window_loudness(SUBBLOCKS_PER_SHORT_TERM)
    }

    /// Loudness of the most recent `n`-sub-block window, gated only by the
    /// absolute `-70 LUFS` threshold (momentary/short-term are ungated by the
    /// relative gate; they return `None` below the absolute gate).
    fn window_loudness(&self, n: usize) -> Option<f64> {
        let energy = self.window_energy(n)?;
        if energy <= 0.0 {
            return None;
        }
        let lufs = energy_to_lufs(energy);
        if lufs < ABSOLUTE_GATE_LUFS {
            None
        } else {
            Some(lufs)
        }
    }

    /// Iterate the energies of all complete 400 ms gating blocks (step 100 ms,
    /// 75 % overlap). Each block energy is the mean of its four sub-blocks.
    fn gating_block_energies(&self) -> Vec<f64> {
        let blocks = &self.subblocks;
        if blocks.len() < SUBBLOCKS_PER_BLOCK {
            return Vec::new();
        }
        let count = blocks.len() - SUBBLOCKS_PER_BLOCK + 1;
        let mut out = Vec::with_capacity(count);
        for start in 0..count {
            if let Some(window) = blocks.get(start..start + SUBBLOCKS_PER_BLOCK) {
                let sum: f64 = window.iter().sum();
                out.push(sum / count_f64(SUBBLOCKS_PER_BLOCK));
            }
        }
        out
    }

    /// Integrated (gated) loudness over all audio seen so far, in LUFS.
    ///
    /// Returns `None` if no gating block survives the absolute `-70 LUFS` gate
    /// (e.g. pure silence, or less than 400 ms of audio).
    #[must_use]
    pub fn integrated(&self) -> Option<f64> {
        let energies = self.gating_block_energies();
        if energies.is_empty() {
            return None;
        }

        // Absolute gate: keep blocks at or above -70 LUFS.
        let abs_gated: Vec<f64> = energies
            .iter()
            .copied()
            .filter(|&e| e > 0.0 && energy_to_lufs(e) >= ABSOLUTE_GATE_LUFS)
            .collect();
        if abs_gated.is_empty() {
            return None;
        }

        // Relative gate: 10 LU below the mean loudness of the abs-gated blocks.
        let abs_mean: f64 = abs_gated.iter().sum::<f64>() / count_f64(abs_gated.len());
        let relative_gate = energy_to_lufs(abs_mean) + RELATIVE_GATE_OFFSET_LU;

        let rel_gated: Vec<f64> = abs_gated
            .iter()
            .copied()
            .filter(|&e| energy_to_lufs(e) >= relative_gate)
            .collect();
        if rel_gated.is_empty() {
            return None;
        }

        let mean: f64 = rel_gated.iter().sum::<f64>() / count_f64(rel_gated.len());
        Some(energy_to_lufs(mean))
    }

    /// Loudness Range (LRA) in LU, per EBU Tech 3342.
    ///
    /// Computed from the distribution of short-term (3 s) loudness values
    /// sampled every 100 ms: absolute-gate at `-70 LUFS`, relative-gate `20 LU`
    /// below the gated mean, then `P95 − P10` of the surviving values. Returns
    /// `None` if fewer than one short-term window of audio exists or nothing
    /// survives the gate.
    #[must_use]
    pub fn loudness_range(&self) -> Option<f64> {
        // Short-term loudness sampled at every sub-block boundary.
        if self.subblocks.len() < SUBBLOCKS_PER_SHORT_TERM {
            return None;
        }
        let last_start = self.subblocks.len() - SUBBLOCKS_PER_SHORT_TERM;
        let mut st_lufs: Vec<f64> = Vec::with_capacity(last_start + 1);
        for start in 0..=last_start {
            if let Some(window) = self.subblocks.get(start..start + SUBBLOCKS_PER_SHORT_TERM) {
                let energy = window.iter().sum::<f64>() / count_f64(SUBBLOCKS_PER_SHORT_TERM);
                if energy > 0.0 {
                    let lufs = energy_to_lufs(energy);
                    if lufs >= ABSOLUTE_GATE_LUFS {
                        st_lufs.push(lufs);
                    }
                }
            }
        }
        if st_lufs.is_empty() {
            return None;
        }

        // Relative gate at -20 LU below the gated mean (computed from energies).
        // Mean of the gated short-term *energies*, expressed back in LUFS.
        let mean_energy: f64 = st_lufs
            .iter()
            .map(|&l| 10f64.powf((l - LOUDNESS_OFFSET) / 10.0))
            .sum::<f64>()
            / count_f64(st_lufs.len());
        let relative_gate = energy_to_lufs(mean_energy) + LRA_RELATIVE_GATE_OFFSET_LU;

        let mut gated: Vec<f64> = st_lufs
            .into_iter()
            .filter(|&l| l >= relative_gate)
            .collect();
        if gated.is_empty() {
            return None;
        }
        gated.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));

        let p10 = percentile(&gated, 0.10);
        let p95 = percentile(&gated, 0.95);
        Some(p95 - p10)
    }

    /// The maximum true-peak across all channels, in dBTP, or `None` if no
    /// non-zero sample has been seen.
    #[must_use]
    pub fn true_peak_dbtp(&self) -> Option<f64> {
        let mut max = 0.0f64;
        let mut any = false;
        for tp in &self.true_peak {
            let p = tp.peak_linear();
            if p > 0.0 {
                any = true;
                if p > max {
                    max = p;
                }
            }
        }
        if any {
            Some(20.0 * max.log10())
        } else {
            None
        }
    }
}

/// Floor of a finite non-negative `f64` as a `usize`, saturating. The rank
/// values fed here are bounded by the slice length (well under `2^53`).
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // reason: rank is finite and in [0, len-1]; truncation/sign loss impossible.
fn floor_usize(x: f64) -> usize {
    if x <= 0.0 {
        0
    } else {
        x.floor() as usize
    }
}

/// Linear-interpolated percentile of a pre-sorted (ascending) slice.
///
/// `q` is in `[0, 1]`. Empty slices return `0.0` (callers gate on non-empty).
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    if last == 0 {
        return sorted.first().copied().unwrap_or(0.0);
    }
    let rank = q.clamp(0.0, 1.0) * count_f64(last);
    let lo_idx = floor_usize(rank).min(last);
    let hi_idx = (lo_idx + 1).min(last);
    let lo_v = sorted.get(lo_idx).copied().unwrap_or(0.0);
    let hi_v = sorted.get(hi_idx).copied().unwrap_or(0.0);
    let frac = rank - count_f64(lo_idx);
    lo_v + (hi_v - lo_v) * frac
}
