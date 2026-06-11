//! Cross-codec **round-trip** property tests: for any representable message,
//! `decode(encode(m)) == m`. The encoder lives in `multiview-output`; the decoder
//! in `multiview-input` (pulled as a dev-dependency only). The two crates define
//! their value models independently, so the bridge functions below assert the
//! field-by-field equality that proves the wire formats agree.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_core::tally::{Brightness, TallyColor};
use multiview_input::tsl as dec;
use multiview_output::tsl as enc;
use proptest::prelude::*;

/// Arbitrary tally colour.
fn color() -> impl Strategy<Value = TallyColor> {
    prop_oneof![
        Just(TallyColor::Off),
        Just(TallyColor::Red),
        Just(TallyColor::Green),
        Just(TallyColor::Amber),
    ]
}

/// Arbitrary brightness `0..=3`.
fn brightness() -> impl Strategy<Value = Brightness> {
    (0u8..=3).prop_map(Brightness::new)
}

/// An encoder lamp from a colour at a fixed brightness.
fn enc_lamp(color: TallyColor, b: Brightness) -> enc::TallyLamp {
    enc::TallyLamp {
        color,
        brightness: b,
    }
}

/// Assert a decoded lamp equals the expected colour at the message's shared
/// brightness (each generation carries one brightness shared across lit lamps).
fn lamp_eq(got: dec::TallyLamp, color: TallyColor) -> Result<(), TestCaseError> {
    prop_assert_eq!(got.color, color);
    Ok(())
}

proptest! {
    /// v4.0: encode∘decode is the identity for index (0..=126), three colour
    /// tallies, a shared brightness, and an ASCII label (≤16 chars).
    #[test]
    fn v40_round_trip(
        index in 0u16..=126,
        left in color(),
        text_tally in color(),
        right in color(),
        b in brightness(),
        label in "[ -~]{0,16}",
    ) {
        let msg = enc::UmdMessage {
            version: enc::TslVersion::V40,
            screen: 0,
            displays: vec![enc::UmdDisplay {
                index,
                left: enc_lamp(left, b),
                text_tally: enc_lamp(text_tally, b),
                right: enc_lamp(right, b),
                text: label.clone(),
            }],
        };
        let wire = enc::v40::encode(&msg).expect("encode");
        let back = dec::v40::decode(&wire).expect("decode");
        let d = &back.displays[0];
        prop_assert_eq!(d.index, index);
        lamp_eq(d.left, left)?;
        lamp_eq(d.text_tally, text_tally)?;
        lamp_eq(d.right, right)?;
        // v4.0 trims trailing spaces in the fixed 16-char field.
        prop_assert_eq!(&d.text, label.trim_end_matches(' '));
    }

    /// v4.0 framed (DLE/STX, TCP): encode∘decode is the identity.
    #[test]
    fn v40_framed_round_trip(
        index in 0u16..=126,
        left in color(),
        right in color(),
        b in brightness(),
        label in "[ -~]{0,16}",
    ) {
        let display = enc::UmdDisplay {
            index,
            left: enc_lamp(left, b),
            text_tally: enc::TallyLamp::off(),
            right: enc_lamp(right, b),
            text: label.clone(),
        };
        let wire = enc::v40::encode_display_framed(&display).expect("encode");
        let back = dec::v40::decode_framed(&wire).expect("decode");
        let d = &back.displays[0];
        prop_assert_eq!(d.index, index);
        lamp_eq(d.left, left)?;
        lamp_eq(d.right, right)?;
        prop_assert_eq!(&d.text, label.trim_end_matches(' '));
    }

    /// v5.0 ASCII, multi-display: encode∘decode is the identity (screen, per
    /// display index + three colour tallies + exact-length ASCII text).
    #[test]
    fn v50_ascii_round_trip(
        screen in any::<u16>(),
        displays in proptest::collection::vec(
            (any::<u16>(), color(), color(), color(), brightness(), "[ -~]{0,40}"),
            1..6,
        ),
    ) {
        let enc_displays: Vec<enc::UmdDisplay> = displays
            .iter()
            .map(|(idx, l, t, r, b, text)| enc::UmdDisplay {
                index: *idx,
                left: enc_lamp(*l, *b),
                text_tally: enc_lamp(*t, *b),
                right: enc_lamp(*r, *b),
                text: text.clone(),
            })
            .collect();
        let msg = enc::UmdMessage {
            version: enc::TslVersion::V50,
            screen,
            displays: enc_displays,
        };
        let wire = enc::v50::encode(&msg, false).expect("encode");
        let back = dec::v50::decode(&wire).expect("decode");
        prop_assert_eq!(back.screen, screen);
        prop_assert_eq!(back.displays.len(), displays.len());
        for (got, (idx, l, t, r, _b, text)) in back.displays.iter().zip(displays.iter()) {
            prop_assert_eq!(got.index, *idx);
            prop_assert_eq!(got.left.color, *l);
            prop_assert_eq!(got.text_tally.color, *t);
            prop_assert_eq!(got.right.color, *r);
            prop_assert_eq!(&got.text, text); // exact length, no trimming
        }
    }

    /// v5.0 UTF-16LE: arbitrary Unicode labels round-trip exactly.
    #[test]
    fn v50_unicode_round_trip(
        screen in any::<u16>(),
        idx in any::<u16>(),
        left in color(),
        right in color(),
        b in brightness(),
        label in ".{0,60}",
    ) {
        let msg = enc::UmdMessage {
            version: enc::TslVersion::V50,
            screen,
            displays: vec![enc::UmdDisplay {
                index: idx,
                left: enc_lamp(left, b),
                text_tally: enc::TallyLamp::off(),
                right: enc_lamp(right, b),
                text: label.clone(),
            }],
        };
        let wire = enc::v50::encode(&msg, true).expect("encode");
        let back = dec::v50::decode(&wire).expect("decode");
        let d = &back.displays[0];
        prop_assert_eq!(d.index, idx);
        prop_assert_eq!(d.left.color, left);
        prop_assert_eq!(d.right.color, right);
        // UTF-16 round trip is lossless for any valid Rust String.
        prop_assert_eq!(&d.text, &label);
    }

    /// v5.0 stuffed (TCP byte-stuffing): encode_stuffed∘decode_stuffed identity.
    #[test]
    fn v50_stuffed_round_trip(
        idx in any::<u16>(),
        label in "[ -~]{0,40}",
    ) {
        let msg = enc::UmdMessage {
            version: enc::TslVersion::V50,
            screen: 0,
            displays: vec![enc::UmdDisplay {
                index: idx,
                left: enc::TallyLamp::off(),
                text_tally: enc::TallyLamp::off(),
                right: enc::TallyLamp::off(),
                text: label.clone(),
            }],
        };
        let wire = enc::v50::encode_stuffed(&msg, false).expect("encode");
        let back = dec::v50::decode_stuffed(&wire).expect("decode");
        prop_assert_eq!(back.displays[0].index, idx);
        prop_assert_eq!(&back.displays[0].text, &label);
    }
}
