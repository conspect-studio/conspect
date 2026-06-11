//! Integration tests for the shared tally vocabulary (`tally` module).
//!
//! These pin the broadcast tally foundation (broadcast-multiviewer brief §2):
//! the TSL UMD colour palette (0/1/2/3 = off/red/green/amber), brightness,
//! bus-source kinds, and the per-tile `TallyState`. Pure value types consumed
//! later by `multiview-overlay`, `multiview-engine`, and `multiview-control`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};

#[test]
fn tally_color_default_is_off() {
    assert_eq!(TallyColor::default(), TallyColor::Off);
    assert!(!TallyColor::default().is_lit());
}

#[test]
fn tally_color_tsl_codes_match_palette() {
    // TSL UMD palette: 0=off, 1=red, 2=green, 3=amber.
    assert_eq!(TallyColor::Off.tsl_code(), 0);
    assert_eq!(TallyColor::Red.tsl_code(), 1);
    assert_eq!(TallyColor::Green.tsl_code(), 2);
    assert_eq!(TallyColor::Amber.tsl_code(), 3);
}

#[test]
fn tally_color_from_tsl_code_round_trips() {
    for color in [
        TallyColor::Off,
        TallyColor::Red,
        TallyColor::Green,
        TallyColor::Amber,
    ] {
        assert_eq!(TallyColor::from_tsl_code(color.tsl_code()), Some(color));
    }
    // Out-of-range code yields None.
    assert_eq!(TallyColor::from_tsl_code(4), None);
}

#[test]
fn lit_colors_report_lit() {
    assert!(TallyColor::Red.is_lit());
    assert!(TallyColor::Green.is_lit());
    assert!(TallyColor::Amber.is_lit());
    assert!(!TallyColor::Off.is_lit());
}

#[test]
fn brightness_clamps_into_two_bit_range() {
    // TSL v3.1 carries 2-bit brightness (0..=3).
    assert_eq!(Brightness::new(0).level(), 0);
    assert_eq!(Brightness::new(3).level(), 3);
    // Saturates above the 2-bit ceiling.
    assert_eq!(Brightness::new(7).level(), 3);
}

#[test]
fn brightness_default_is_full() {
    assert_eq!(Brightness::default(), Brightness::FULL);
    assert_eq!(Brightness::FULL.level(), 3);
}

#[test]
fn tally_state_default_is_off() {
    let state = TallyState::default();
    assert_eq!(state.color, TallyColor::Off);
    assert!(!state.is_lit());
}

#[test]
fn tally_state_program_is_red_preview_is_green() {
    let prog = TallyState::program();
    assert_eq!(prog.color, TallyColor::Red);
    assert!(prog.is_lit());

    let prev = TallyState::preview();
    assert_eq!(prev.color, TallyColor::Green);
    assert!(prev.is_lit());
}

#[test]
fn bus_source_round_trips_tagged() {
    for bus in [
        BusSource::Program,
        BusSource::Preview,
        BusSource::Aux { index: 2 },
        BusSource::Iso { index: 5 },
    ] {
        let json = serde_json::to_string(&bus).unwrap();
        // Tagged (not untagged): the discriminant key is present.
        assert!(json.contains("kind"), "json was: {json}");
        let back: BusSource = serde_json::from_str(&json).unwrap();
        assert_eq!(bus, back);
    }
}

#[test]
fn tally_state_round_trips_via_json() {
    let state = TallyState {
        color: TallyColor::Amber,
        brightness: Brightness::new(2),
        source: BusSource::Aux { index: 1 },
    };
    let json = serde_json::to_string(&state).unwrap();
    let back: TallyState = serde_json::from_str(&json).unwrap();
    assert_eq!(state, back);
}
