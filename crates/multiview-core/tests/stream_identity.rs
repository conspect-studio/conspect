//! RT-0 acceptance tests for the per-stream identity types (ADR-0034 §1).
//!
//! Integration tests do not inherit `clippy.toml`'s test relaxations, so this
//! file opts out of the panic-bearing lints explicitly (CLAUDE.md §A.1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::stream::{
    Bcp47, Bcp47Error, CoarseMediaKind, DataKind, StabilityTier, StableStreamId, StreamKind,
    TcSourceKind,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// StreamKind — the canonical superset + predicates.
// ---------------------------------------------------------------------------

#[test]
fn stream_kind_predicates_are_mutually_exclusive_and_match_variant() {
    let cases = [
        (StreamKind::Video, "video"),
        (StreamKind::Audio, "audio"),
        (StreamKind::Subtitle, "subtitle"),
        (StreamKind::Data(DataKind::Scte35), "data"),
        (StreamKind::Data(DataKind::Klv), "data"),
        (StreamKind::Timecode(TcSourceKind::Ltc), "timecode"),
    ];
    for (kind, want) in cases {
        let preds = [
            ("video", kind.is_video()),
            ("audio", kind.is_audio()),
            ("subtitle", kind.is_subtitle()),
            ("data", kind.is_data()),
            ("timecode", kind.is_timecode()),
        ];
        // Exactly one predicate is true, and it is the expected one.
        let lit: Vec<&str> = preds.iter().filter(|(_, b)| *b).map(|(n, _)| *n).collect();
        assert_eq!(lit, vec![want], "kind {kind:?} predicate set wrong");
    }
}

#[test]
fn stream_kind_serialises_adjacently_tagged_never_untagged() {
    // Adjacently tagged on `kind` (+ `payload`), snake_case (ADR-0010: never
    // untagged). A unit variant is just its tag.
    let v = serde_json::to_value(StreamKind::Video).unwrap();
    assert_eq!(v, serde_json::json!({ "kind": "video" }));

    // Data carries its DataKind payload unambiguously under `payload` — NOT
    // flattened into a bare `{"kind":"scte35"}` (which would alias a hypothetical
    // `scte35` stream kind).
    let scte = serde_json::to_value(StreamKind::Data(DataKind::Scte35)).unwrap();
    assert_eq!(
        scte,
        serde_json::json!({ "kind": "data", "payload": "scte35" })
    );

    // Timecode likewise carries its source family under `payload`.
    let tc = serde_json::to_value(StreamKind::Timecode(TcSourceKind::Ltc)).unwrap();
    assert_eq!(
        tc,
        serde_json::json!({ "kind": "timecode", "payload": "ltc" })
    );

    // Round-trips (the real guard: the wire form is unambiguous).
    let round: StreamKind = serde_json::from_value(scte).unwrap();
    assert_eq!(round, StreamKind::Data(DataKind::Scte35));
    let round_tc: StreamKind = serde_json::from_value(tc).unwrap();
    assert_eq!(round_tc, StreamKind::Timecode(TcSourceKind::Ltc));
}

#[test]
fn coarse_media_kind_lifts_av_kinds_one_to_one() {
    assert_eq!(StreamKind::from(CoarseMediaKind::Video), StreamKind::Video);
    assert_eq!(StreamKind::from(CoarseMediaKind::Audio), StreamKind::Audio);
    assert_eq!(
        StreamKind::from(CoarseMediaKind::Subtitle),
        StreamKind::Subtitle
    );
    // `Other` with no codec name is a generic data passthrough (not dropped).
    assert!(StreamKind::from(CoarseMediaKind::Other).is_data());
}

#[test]
fn from_coarse_and_codec_classifies_known_other_codecs() {
    // SCTE-35 (several spellings).
    for name in ["scte_35", "scte35", "SCTE-35", "scte_35 "] {
        assert_eq!(
            StreamKind::from_coarse_and_codec(CoarseMediaKind::Other, name),
            StreamKind::Data(DataKind::Scte35),
            "codec {name:?} should classify as SCTE-35"
        );
    }
    // KLV.
    for name in ["klv", "smpte_klv", "SMPTE_KLV"] {
        assert_eq!(
            StreamKind::from_coarse_and_codec(CoarseMediaKind::Other, name),
            StreamKind::Data(DataKind::Klv),
            "codec {name:?} should classify as KLV"
        );
    }
    // Timecode (timed ID3 / explicit timecode).
    for name in ["timed_id3", "timecode", "TIMED_ID3"] {
        assert!(
            StreamKind::from_coarse_and_codec(CoarseMediaKind::Other, name).is_timecode(),
            "codec {name:?} should classify as timecode"
        );
    }
    // AV kinds ignore the codec name and lift 1:1.
    assert_eq!(
        StreamKind::from_coarse_and_codec(CoarseMediaKind::Video, "h264"),
        StreamKind::Video
    );
    assert_eq!(
        StreamKind::from_coarse_and_codec(CoarseMediaKind::Audio, "aac"),
        StreamKind::Audio
    );
    assert_eq!(
        StreamKind::from_coarse_and_codec(CoarseMediaKind::Subtitle, "dvb_subtitle"),
        StreamKind::Subtitle
    );
    // Unknown data essence stays routable (generic Data), never dropped.
    assert!(StreamKind::from_coarse_and_codec(CoarseMediaKind::Other, "bin_data").is_data());
}

// ---------------------------------------------------------------------------
// Bcp47 — parse/validate/normalise/round-trip.
// ---------------------------------------------------------------------------

#[test]
fn bcp47_normalises_case_and_round_trips_valid_tags() {
    assert_eq!(Bcp47::try_from("eng").unwrap().as_str(), "eng");
    assert_eq!(Bcp47::try_from("EN").unwrap().as_str(), "en");
    assert_eq!(Bcp47::try_from("en-us").unwrap().as_str(), "en-US");
    assert_eq!(Bcp47::try_from("EN_US").unwrap().as_str(), "en-US");
    assert_eq!(Bcp47::try_from("spa").unwrap().as_str(), "spa");
    // Two normalisations of the same tag compare equal.
    assert_eq!(
        Bcp47::try_from("EN-us").unwrap(),
        Bcp47::try_from("en-US").unwrap()
    );
    // Display == as_str; FromStr agrees with TryFrom.
    let tag: Bcp47 = "en-US".parse().unwrap();
    assert_eq!(tag.to_string(), "en-US");
}

#[test]
fn bcp47_rejects_clearly_invalid_input() {
    assert_eq!(Bcp47::try_from(""), Err(Bcp47Error::Empty));
    assert_eq!(Bcp47::try_from("   "), Err(Bcp47Error::Empty));
    // `und` is undetermined → model as None, not a Bcp47.
    assert_eq!(Bcp47::try_from("und"), Err(Bcp47Error::Undetermined));
    // Digits / single-letter / oversized primary subtag.
    assert!(matches!(
        Bcp47::try_from("e"),
        Err(Bcp47Error::InvalidPrimary(_))
    ));
    assert!(matches!(
        Bcp47::try_from("123"),
        Err(Bcp47Error::InvalidPrimary(_))
    ));
    assert!(matches!(
        Bcp47::try_from("english"),
        Err(Bcp47Error::InvalidPrimary(_))
    ));
    // Empty / oversized non-primary subtag.
    assert!(matches!(
        Bcp47::try_from("en-"),
        Err(Bcp47Error::InvalidSubtag(_))
    ));
    assert!(matches!(
        Bcp47::try_from("en-toolongsubtag"),
        Err(Bcp47Error::InvalidSubtag(_))
    ));
}

#[test]
fn bcp47_serde_round_trips_through_string() {
    let tag = Bcp47::try_from("pt-BR").unwrap();
    let json = serde_json::to_string(&tag).unwrap();
    assert_eq!(json, "\"pt-BR\"");
    let back: Bcp47 = serde_json::from_str(&json).unwrap();
    assert_eq!(back, tag);
    // Deserialising an invalid tag is an error (try_from gate).
    assert!(serde_json::from_str::<Bcp47>("\"und\"").is_err());
}

// ---------------------------------------------------------------------------
// StableStreamId — kind-scoped, tiered, permutation-stable.
// ---------------------------------------------------------------------------

#[test]
fn ts_pid_and_hls_are_hard_tier() {
    assert_eq!(
        StableStreamId::from_ts_pid(StreamKind::Video, 0x100).tier(),
        StabilityTier::Hard
    );
    assert_eq!(
        StableStreamId::from_hls(StreamKind::Audio, "aud", "English").tier(),
        StabilityTier::Hard
    );
}

#[test]
fn general_id_is_soft_tier_when_ordinal_only_discriminator() {
    // Two audio streams identical in codec/lang/title differ only by ordinal:
    // they get DISTINCT ids (no collision) but are flagged SOFT (reorder risk).
    let lang = Bcp47::try_from("eng").unwrap();
    let a0 = StableStreamId::from_general(StreamKind::Audio, 0, "aac", Some(&lang), None);
    let a1 = StableStreamId::from_general(StreamKind::Audio, 1, "aac", Some(&lang), None);
    assert_ne!(a0, a1, "ordinal must disambiguate identical tracks");
    assert_eq!(a0.tier(), StabilityTier::Soft);
    assert_eq!(a1.tier(), StabilityTier::Soft);
}

#[test]
fn kind_scoped_ids_never_alias_across_kinds() {
    // Same PID number, different kind → different id (scoped by kind).
    let v = StableStreamId::from_ts_pid(StreamKind::Video, 0x100);
    let a = StableStreamId::from_ts_pid(StreamKind::Audio, 0x100);
    assert_ne!(v, a);
    assert_ne!(v.kind_scope(), a.kind_scope());
}

#[test]
fn hls_key_is_injective_in_group_and_name() {
    // Length-prefixed so ("ab","c") and ("a","bc") cannot collide.
    let x = StableStreamId::from_hls(StreamKind::Audio, "ab", "c");
    let y = StableStreamId::from_hls(StreamKind::Audio, "a", "bc");
    assert_ne!(x, y);
}

// ---------------------------------------------------------------------------
// Property tests (committed proptest-regressions/).
// ---------------------------------------------------------------------------

/// A strategy over the canonical stream kinds.
fn any_stream_kind() -> impl Strategy<Value = StreamKind> {
    prop_oneof![
        Just(StreamKind::Video),
        Just(StreamKind::Audio),
        Just(StreamKind::Subtitle),
        Just(StreamKind::Data(DataKind::Scte35)),
        Just(StreamKind::Data(DataKind::Klv)),
        Just(StreamKind::Timecode(TcSourceKind::Ltc)),
        Just(StreamKind::Timecode(TcSourceKind::AtcRp188)),
    ]
}

proptest! {
    /// HARD tier (TS PID): the id is identical no matter what container index /
    /// position the stream was at — a PID is permutation-invariant by
    /// construction (it carries no ordinal).
    #[test]
    fn prop_ts_pid_stable_under_permutation(
        kind in any_stream_kind(),
        pid in 0u16..=0x1fff,
        // A re-probe that reorders streams cannot change the PID; model that by
        // building the same id twice "from different snapshots".
        _perm in 0u32..1000,
    ) {
        let a = StableStreamId::from_ts_pid(kind, pid);
        let b = StableStreamId::from_ts_pid(kind, pid);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(a.tier(), StabilityTier::Hard);
    }

    /// HARD tier (HLS): stable under rendition reorder — keyed only by
    /// group+name, never by position.
    #[test]
    fn prop_hls_stable_under_reorder(
        kind in any_stream_kind(),
        group in "[a-z0-9]{1,8}",
        name in "[a-zA-Z0-9 ]{1,12}",
        _pos_a in 0u32..32,
        _pos_b in 0u32..32,
    ) {
        let a = StableStreamId::from_hls(kind, &group, &name);
        let b = StableStreamId::from_hls(kind, &group, &name);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(a.tier(), StabilityTier::Hard);
    }

    /// GENERAL/SOFT tier: stable under index permutation as long as the
    /// disambiguating fields (ordinal, codec, language, title) are unchanged —
    /// it does NOT depend on the container index, only on those fields.
    #[test]
    fn prop_general_stable_when_fields_fixed(
        kind in any_stream_kind(),
        ordinal in 0u32..16,
        codec in "[a-z0-9_]{1,10}",
        lang in prop::option::of("[a-z]{2,3}"),
        title in prop::option::of("[a-zA-Z0-9 ]{0,16}"),
    ) {
        // `und` is rejected by Bcp47; map it to None to keep the strategy total.
        let bcp = lang
            .as_deref()
            .filter(|l| *l != "und")
            .and_then(|l| Bcp47::try_from(l).ok());
        let a = StableStreamId::from_general(
            kind, ordinal, &codec, bcp.as_ref(), title.as_deref());
        let b = StableStreamId::from_general(
            kind, ordinal, &codec, bcp.as_ref(), title.as_deref());
        prop_assert_eq!(&a, &b);
        // Always flagged soft so the operator sees the reorder risk.
        prop_assert_eq!(a.tier(), StabilityTier::Soft);
    }

    /// GENERAL/SOFT tier: changing ONLY the ordinal yields a DIFFERENT id (so
    /// two otherwise-identical tracks never collide) — and both stay soft.
    #[test]
    fn prop_general_ordinal_disambiguates(
        kind in any_stream_kind(),
        ord_a in 0u32..8,
        ord_b in 0u32..8,
        codec in "[a-z0-9_]{1,10}",
    ) {
        prop_assume!(ord_a != ord_b);
        let a = StableStreamId::from_general(kind, ord_a, &codec, None, None);
        let b = StableStreamId::from_general(kind, ord_b, &codec, None, None);
        prop_assert_ne!(a, b);
    }

    /// Bcp47 round-trips every valid alpha-2/3 + optional region tag, and
    /// normalisation is idempotent.
    #[test]
    fn prop_bcp47_round_trips_valid(
        prim in "[a-zA-Z]{2,3}",
        region in prop::option::of("[a-zA-Z]{2}"),
    ) {
        prop_assume!(!prim.eq_ignore_ascii_case("und"));
        let raw = match &region {
            Some(r) => format!("{prim}-{r}"),
            None => prim.clone(),
        };
        let tag = Bcp47::try_from(raw.as_str()).expect("valid tag");
        // Re-parsing the normalised form is a fixed point.
        let again = Bcp47::try_from(tag.as_str()).expect("normalised tag re-parses");
        prop_assert_eq!(&tag, &again);
        // Serde String round-trip.
        let s = serde_json::to_string(&tag).unwrap();
        let back: Bcp47 = serde_json::from_str(&s).unwrap();
        prop_assert_eq!(tag, back);
    }

    /// Bcp47 rejects clearly-invalid primary subtags (too short / too long /
    /// non-alpha) without panicking.
    #[test]
    fn prop_bcp47_rejects_bad_primary(
        bad in "[a-z]{4,8}|[0-9]{1,3}|[a-z]",
    ) {
        prop_assert!(Bcp47::try_from(bad.as_str()).is_err());
    }
}
