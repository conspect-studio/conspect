//! In-band parameter-set / Annex-B framing **filter selection** (GP-3, ADR-0030
//! §4 "Framing prerequisite"). Pure decision logic, **always compiled**.
//!
//! A guarded passthrough splices a pre-baked slate into a *copied* elementary
//! stream. For a continuous-ES splice on MPEG-TS / SRT / RTSP-RTP / raw-Annex-B
//! targets the active parameter sets (H.264 SPS/PPS, HEVC VPS/SPS/PPS) **must**
//! be repeated **in-band** immediately before **both** the slate IDR and the
//! recovery IDR, so a decoder that joins (or re-acquires across the seam) can
//! decode without an out-of-band config record. `h264_mp4toannexb` alone inserts
//! the parameter sets only at stream-start / on an extradata change, which is
//! insufficient for a mid-stream splice — hence a dedicated `dump_extra` /
//! `extract_extradata` stage.
//!
//! This module owns the **pure** half of GP-3: given a codec and the desired
//! output framing, it composes the ordered list of libav bitstream-filter names
//! that the FFI [`crate::bsf::BsfChain`] then instantiates. Keeping the selection
//! pure means it is exhaustively unit-testable in the default (no-libav) build;
//! the FFI module merely executes the plan this module produces.
//!
//! ## The framing contract
//!
//! Given access units that arrive **either** length-prefixed (avcC/hvcC) **or**
//! already Annex-B, the [`BsfFraming::AnnexBInBand`] plan yields Annex-B-framed
//! output with the active SPS/PPS(/VPS) repeated **in-band before every
//! keyframe**, so the copied-input side and the slate side reach the muxer in
//! identical, decoder-reacquirable framing.

use crate::idr::CodecKind;

/// The desired **output** framing of a [`crate::bsf::BsfChain`].
///
/// A continuous-ES splice target (MPEG-TS / SRT / RTSP-RTP / raw H.264·HEVC)
/// needs [`AnnexBInBand`](BsfFraming::AnnexBInBand); a length-prefixed container
/// (mp4 / fMP4) keeps its avcC/hvcC framing and carries parameter sets in the
/// container header, so it needs only the in-band-repeat stage without the
/// Annex-B conversion ([`LengthPrefixedInBand`](BsfFraming::LengthPrefixedInBand)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BsfFraming {
    /// Annex-B (start-code) framing with the active parameter sets repeated
    /// in-band before **every** keyframe. The framing every continuous-ES splice
    /// target requires (and the one the slate baker emits), so the copied input
    /// and the slate are byte-for-byte framing-identical at the muxer.
    AnnexBInBand,
    /// Keep length-prefixed (avcC/hvcC) framing but still repeat the parameter
    /// sets in-band before every keyframe (for fMP4 reusing a single
    /// `EXT-X-MAP`, ADR-0030 rung 1).
    LengthPrefixedInBand,
}

/// The maximum number of filters a GP-3 chain ever composes.
///
/// At most an Annex-B converter plus the in-band-repeat stage; sized so callers
/// can stack-allocate the plan without a heap `Vec`.
pub const MAX_BSF_CHAIN: usize = 2;

/// An ordered, fixed-capacity list of libav bitstream-filter names.
///
/// The output of [`plan_bsf_chain`]; the FFI [`crate::bsf::BsfChain`] instantiates
/// one `AVBSFContext` per name, in order. Empty means "no filtering required"
/// (the input already satisfies the contract — e.g. an AV1 OBU stream, whose
/// sequence header is carried per temporal unit, needs no NAL re-framing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BsfPlan {
    names: [Option<&'static str>; MAX_BSF_CHAIN],
    len: usize,
}

impl BsfPlan {
    /// An empty plan (no filters).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            names: [None; MAX_BSF_CHAIN],
            len: 0,
        }
    }

    /// Build a plan from an ordered slice of filter names (at most
    /// [`MAX_BSF_CHAIN`]; any excess is ignored — never panics).
    #[must_use]
    fn from_names(names: &[&'static str]) -> Self {
        let mut plan = Self::empty();
        for &name in names.iter().take(MAX_BSF_CHAIN) {
            if let Some(slot) = plan.names.get_mut(plan.len) {
                *slot = Some(name);
                plan.len += 1;
            }
        }
        plan
    }

    /// The filter names, in application order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.names.iter().take(self.len).filter_map(|n| *n)
    }

    /// How many filters the plan contains.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the plan is empty (the input already satisfies the contract).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The libav `dump_extra` filter: re-injects the stream's extradata (the active
/// SPS/PPS/VPS) **in-band** ahead of packets. Configured with `freq=keyframes`
/// (alias `k`) so it repeats the parameter sets before **every** keyframe — the
/// in-band-PS-before-IDR guarantee at the heart of GP-3.
pub const FILTER_DUMP_EXTRA: &str = "dump_extra";

/// The libav `extract_extradata` filter: pulls in-band SPS/PPS/VPS **out** of the
/// access units into the stream's extradata side-data. Run **first** when the
/// input is already Annex-B with in-line parameter sets but no avcC/hvcC record,
/// so `dump_extra` has an authoritative extradata to repeat.
pub const FILTER_EXTRACT_EXTRADATA: &str = "extract_extradata";

/// The libav `h264_mp4toannexb` filter: converts avcC length-prefixed H.264 into
/// Annex-B start-code framing (and inserts the SPS/PPS from extradata at
/// stream-start — but **only** there, hence the trailing `dump_extra`).
pub const FILTER_H264_MP4TOANNEXB: &str = "h264_mp4toannexb";

/// The libav `hevc_mp4toannexb` filter: the HEVC sibling of
/// [`FILTER_H264_MP4TOANNEXB`].
pub const FILTER_HEVC_MP4TOANNEXB: &str = "hevc_mp4toannexb";

/// The `freq` value handed to `dump_extra` so it repeats parameter sets before
/// **every keyframe** (not just on an extradata change).
///
/// libav's `dump_extra` (`dump_extradata`) names this enum constant `k` /
/// `keyframe` (value `0`); the spec's `keyframes` is not a libav constant, so the
/// short, unambiguous `k` alias is used.
pub const DUMP_EXTRA_FREQ_KEYFRAMES: &str = "k";

/// Whether the input access units are length-prefixed (avcC/hvcC) or Annex-B.
///
/// Derived from the stream's extradata framing (see
/// [`crate::demux::Demuxer::stream_idr_framing`]); the selection only needs the
/// binary "length-prefixed vs not", so this mirrors that distinction without
/// carrying the length-size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputFraming {
    /// avcC / hvcC length-prefixed NALs (an mp4/mov/fMP4 source).
    LengthPrefixed,
    /// Annex-B start-code framing (an MPEG-TS / RTP / raw source).
    AnnexB,
}

/// Compose the ordered bitstream-filter chain for `(codec, input, desired)`
/// (GP-3, ADR-0030 §4 "Framing prerequisite").
///
/// The plan guarantees the active parameter sets repeat **in-band before every
/// keyframe** in the requested output framing:
///
/// * **H.264 / HEVC, [`BsfFraming::AnnexBInBand`]:**
///   - length-prefixed input → `{ h264|hevc_mp4toannexb, dump_extra }`: convert
///     to Annex-B (which seeds parameter sets at stream-start only), then
///     `dump_extra(freq=keyframes)` repeats them before **every** keyframe.
///   - Annex-B input → `{ extract_extradata, dump_extra }`: lift any in-line
///     parameter sets into extradata, then repeat them before every keyframe.
/// * **H.264 / HEVC, [`BsfFraming::LengthPrefixedInBand`]:** `{ dump_extra }`
///   only — keep avcC/hvcC framing, repeat the parameter sets in-band before
///   every keyframe.
/// * **AV1 or any other codec:** an empty plan — AV1 carries its sequence header
///   per temporal unit (OBU), so no NAL re-framing is required, and an
///   unmodelled codec is passed through untouched.
#[must_use]
pub fn plan_bsf_chain(codec: CodecKind, input: InputFraming, desired: BsfFraming) -> BsfPlan {
    match codec {
        CodecKind::H264 => nal_plan(FILTER_H264_MP4TOANNEXB, input, desired),
        CodecKind::Hevc => nal_plan(FILTER_HEVC_MP4TOANNEXB, input, desired),
        // AV1's sequence header rides per temporal unit; nothing to re-frame.
        CodecKind::Av1 | CodecKind::Other => BsfPlan::empty(),
    }
}

/// Build the H.264 / HEVC chain, parameterised by the codec's annex-b converter.
fn nal_plan(annexb_filter: &'static str, input: InputFraming, desired: BsfFraming) -> BsfPlan {
    match desired {
        BsfFraming::AnnexBInBand => match input {
            // avcC/hvcC → Annex-B converter seeds PS at stream-start; dump_extra
            // then repeats them before every keyframe.
            InputFraming::LengthPrefixed => {
                BsfPlan::from_names(&[annexb_filter, FILTER_DUMP_EXTRA])
            }
            // Already Annex-B: lift any in-line PS to extradata, then repeat.
            InputFraming::AnnexB => {
                BsfPlan::from_names(&[FILTER_EXTRACT_EXTRADATA, FILTER_DUMP_EXTRA])
            }
        },
        // Keep length-prefixed framing, just repeat PS in-band per keyframe.
        BsfFraming::LengthPrefixedInBand => BsfPlan::from_names(&[FILTER_DUMP_EXTRA]),
    }
}

/// Whether a filter name is `dump_extra` and therefore needs its `freq` option
/// set to [`DUMP_EXTRA_FREQ_KEYFRAMES`] at instantiation (GP-3). The FFI module
/// queries this to decide whether to `av_opt_set(priv_data, "freq", …)` after
/// `av_bsf_alloc`.
#[must_use]
pub fn needs_keyframe_freq_option(filter_name: &str) -> bool {
    filter_name == FILTER_DUMP_EXTRA
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        needs_keyframe_freq_option, plan_bsf_chain, BsfFraming, BsfPlan, InputFraming,
        FILTER_DUMP_EXTRA, FILTER_EXTRACT_EXTRADATA, FILTER_H264_MP4TOANNEXB,
        FILTER_HEVC_MP4TOANNEXB, MAX_BSF_CHAIN,
    };
    use crate::idr::CodecKind;

    fn plan_names(plan: &BsfPlan) -> Vec<&'static str> {
        plan.names().collect()
    }

    #[test]
    fn h264_length_prefixed_to_annexb_inserts_converter_then_dump_extra() {
        let plan = plan_bsf_chain(
            CodecKind::H264,
            InputFraming::LengthPrefixed,
            BsfFraming::AnnexBInBand,
        );
        assert_eq!(
            plan_names(&plan),
            vec![FILTER_H264_MP4TOANNEXB, FILTER_DUMP_EXTRA],
            "avcC H.264 → Annex-B: convert then repeat PS per keyframe"
        );
    }

    #[test]
    fn h264_annexb_input_to_annexb_extracts_then_dump_extra() {
        let plan = plan_bsf_chain(
            CodecKind::H264,
            InputFraming::AnnexB,
            BsfFraming::AnnexBInBand,
        );
        assert_eq!(
            plan_names(&plan),
            vec![FILTER_EXTRACT_EXTRADATA, FILTER_DUMP_EXTRA],
            "Annex-B H.264 → lift in-line PS to extradata, then repeat per keyframe"
        );
    }

    #[test]
    fn hevc_uses_the_hevc_annexb_converter() {
        let plan = plan_bsf_chain(
            CodecKind::Hevc,
            InputFraming::LengthPrefixed,
            BsfFraming::AnnexBInBand,
        );
        assert_eq!(
            plan_names(&plan),
            vec![FILTER_HEVC_MP4TOANNEXB, FILTER_DUMP_EXTRA],
            "HEVC must use hevc_mp4toannexb, never the H.264 converter"
        );
    }

    #[test]
    fn length_prefixed_in_band_is_dump_extra_only() {
        for codec in [CodecKind::H264, CodecKind::Hevc] {
            let plan = plan_bsf_chain(
                codec,
                InputFraming::LengthPrefixed,
                BsfFraming::LengthPrefixedInBand,
            );
            assert_eq!(
                plan_names(&plan),
                vec![FILTER_DUMP_EXTRA],
                "keeping avcC/hvcC framing only needs the in-band repeat"
            );
        }
    }

    #[test]
    fn av1_and_other_codecs_need_no_filtering() {
        for codec in [CodecKind::Av1, CodecKind::Other] {
            for input in [InputFraming::AnnexB, InputFraming::LengthPrefixed] {
                let plan = plan_bsf_chain(codec, input, BsfFraming::AnnexBInBand);
                assert!(
                    plan.is_empty(),
                    "AV1 / unmodelled codecs carry their own headers → empty plan"
                );
                assert_eq!(plan.len(), 0);
                assert_eq!(plan_names(&plan), Vec::<&'static str>::new());
            }
        }
    }

    #[test]
    fn every_nal_plan_ends_with_dump_extra_so_ps_repeat_per_keyframe() {
        // The whole point of GP-3: regardless of input framing, an H.264/HEVC
        // Annex-B plan MUST terminate in dump_extra, the only stage that repeats
        // the parameter sets before EVERY keyframe (the converters seed them once).
        for codec in [CodecKind::H264, CodecKind::Hevc] {
            for input in [InputFraming::AnnexB, InputFraming::LengthPrefixed] {
                let plan = plan_bsf_chain(codec, input, BsfFraming::AnnexBInBand);
                let names = plan_names(&plan);
                assert_eq!(
                    names.last().copied(),
                    Some(FILTER_DUMP_EXTRA),
                    "the in-band-PS guarantee comes from a trailing dump_extra"
                );
            }
        }
    }

    #[test]
    fn dump_extra_is_the_only_filter_wanting_the_keyframe_freq_option() {
        assert!(needs_keyframe_freq_option(FILTER_DUMP_EXTRA));
        assert!(!needs_keyframe_freq_option(FILTER_H264_MP4TOANNEXB));
        assert!(!needs_keyframe_freq_option(FILTER_HEVC_MP4TOANNEXB));
        assert!(!needs_keyframe_freq_option(FILTER_EXTRACT_EXTRADATA));
        assert!(!needs_keyframe_freq_option("some_other_bsf"));
    }

    #[test]
    fn plan_never_exceeds_the_fixed_capacity() {
        // A plan must never overflow MAX_BSF_CHAIN regardless of inputs.
        for codec in [
            CodecKind::H264,
            CodecKind::Hevc,
            CodecKind::Av1,
            CodecKind::Other,
        ] {
            for input in [InputFraming::AnnexB, InputFraming::LengthPrefixed] {
                for desired in [BsfFraming::AnnexBInBand, BsfFraming::LengthPrefixedInBand] {
                    let plan = plan_bsf_chain(codec, input, desired);
                    assert!(plan.len() <= MAX_BSF_CHAIN);
                }
            }
        }
    }

    #[test]
    fn empty_plan_helpers_are_consistent() {
        let plan = BsfPlan::empty();
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
        assert_eq!(plan.names().count(), 0);
    }
}
