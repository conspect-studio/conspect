//! GP-6 Piece B — pure (default-build) tests for the muxer-option helper
//! (ADR-0030 §4). These run WITHOUT the `ffmpeg` feature: they pin the typed,
//! libav-free option surface (`MuxOptions`) that the feature-gated `Muxer`
//! consumes — the two known knobs (`avoid_negative_ts`, `max_interleave_delta`)
//! and the rejection of malformed keys/values (interior NUL) before any FFI.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::MuxOptions;

#[test]
fn builder_collects_the_two_known_knobs_in_order() {
    let opts = MuxOptions::new()
        .avoid_negative_ts_make_zero()
        .max_interleave_delta(0);
    let pairs = opts.as_pairs();
    assert_eq!(
        pairs,
        &[
            ("avoid_negative_ts".to_owned(), "make_zero".to_owned()),
            ("max_interleave_delta".to_owned(), "0".to_owned()),
        ]
    );
}

#[test]
fn from_pairs_round_trips_arbitrary_keys() {
    let opts = MuxOptions::from_pairs(&[("avoid_negative_ts", "make_zero"), ("fflags", "+genpts")])
        .expect("valid pairs accepted");
    assert_eq!(opts.as_pairs().len(), 2);
    assert_eq!(
        opts.as_pairs()[1],
        ("fflags".to_owned(), "+genpts".to_owned())
    );
}

#[test]
fn empty_options_is_valid_and_empty() {
    let opts = MuxOptions::new();
    assert!(opts.is_empty());
    assert!(opts.as_pairs().is_empty());
    // from an empty slice too.
    let from_empty = MuxOptions::from_pairs(&[]).expect("empty slice accepted");
    assert!(from_empty.is_empty());
}

#[test]
fn interior_nul_in_key_or_value_is_rejected_before_ffi() {
    // A key/value with an interior NUL can never become a CString for av_dict_set;
    // the pure validator rejects it up front (a typed error, never a panic).
    assert!(MuxOptions::from_pairs(&[("bad\0key", "1")]).is_err());
    assert!(MuxOptions::from_pairs(&[("ok", "bad\0val")]).is_err());
}

#[test]
fn max_interleave_delta_renders_the_integer_value() {
    let opts = MuxOptions::new().max_interleave_delta(7_000_000);
    assert_eq!(
        opts.as_pairs(),
        &[("max_interleave_delta".to_owned(), "7000000".to_owned())]
    );
}
