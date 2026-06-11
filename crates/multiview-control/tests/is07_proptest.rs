//! Property tests for the pure IS-07 codec and the tally mirror/override
//! value-machines in `multiview-control`.
//!
//! These pin the round-trip and last-writer-wins invariants the operator surface
//! relies on, exhaustively over generated inputs (no sockets, no async).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::{
    tally_color_from_is07, tally_color_to_is07, tally_event_to_is07, GpiEvent, Is07Message,
    Is07Payload, OverrideRegistry, TallyMirror,
};
use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use multiview_events::{TallyEvent, TallyTarget};
use proptest::prelude::*;

/// A strategy producing any TSL tally colour.
fn any_color() -> impl Strategy<Value = TallyColor> {
    prop_oneof![
        Just(TallyColor::Off),
        Just(TallyColor::Red),
        Just(TallyColor::Green),
        Just(TallyColor::Amber),
    ]
}

/// A strategy producing any tally target.
fn any_target() -> impl Strategy<Value = TallyTarget> {
    prop_oneof![
        any::<u32>().prop_map(|index| TallyTarget::Tile { index }),
        "[a-z0-9-]{1,12}".prop_map(|name| TallyTarget::Element { name }),
    ]
}

proptest! {
    /// Every TSL colour survives the IS-07 `number` round-trip exactly.
    #[test]
    fn is07_tally_color_round_trips(color in any_color()) {
        let msg = tally_color_to_is07(color, "1700000000:0");
        prop_assert_eq!(tally_color_from_is07(&msg), Some(color));
    }

    /// A Boolean GPI bit survives the IS-07 round-trip exactly (line + value).
    #[test]
    fn is07_gpi_round_trips(line in "[a-z0-9-]{1,16}", asserted in any::<bool>()) {
        let gpi = GpiEvent { line, asserted };
        let msg = gpi.to_is07("1700000000:0");
        let back = GpiEvent::from_is07(&msg).expect("boolean state decodes to a GPI event");
        prop_assert_eq!(back, gpi);
    }

    /// A resolved tally event always renders to an IS-07 `number` state message
    /// whose value is the colour's TSL palette code.
    #[test]
    fn tally_event_renders_palette_code(target in any_target(), color in any_color()) {
        let state = TallyState {
            color,
            brightness: Brightness::FULL,
            source: BusSource::Program,
        };
        let event = TallyEvent { target, state };
        let msg = tally_event_to_is07(&event, "1700000000:0");
        match msg {
            Is07Message::State { payload: Is07Payload::Number { value, scale }, .. } => {
                prop_assert_eq!(scale, 1);
                prop_assert_eq!(value, i64::from(color.tsl_code()));
            }
            other => prop_assert!(false, "expected number state, got {:?}", other),
        }
    }

    /// The tally mirror is last-writer-wins per target: after applying a sequence
    /// of states for one target, `get` returns the last one applied.
    #[test]
    fn mirror_is_last_writer_wins(
        target in any_target(),
        colors in proptest::collection::vec(any_color(), 1..16),
    ) {
        let mirror = TallyMirror::new();
        let mut last = TallyColor::Off;
        for color in colors {
            let state = TallyState {
                color,
                brightness: Brightness::FULL,
                source: BusSource::Preview,
            };
            mirror.apply(TallyEvent { target: target.clone(), state });
            last = color;
        }
        // Exactly one entry for the single target, holding the last colour.
        prop_assert_eq!(mirror.len(), 1);
        let got = mirror.get(&target).expect("the target was applied");
        prop_assert_eq!(got.state.color, last);
    }

    /// The override registry is last-writer-wins and `clear` always empties a
    /// previously-set target.
    #[test]
    fn override_registry_set_then_clear(
        target in any_target(),
        colors in proptest::collection::vec(any_color(), 1..8),
    ) {
        let reg = OverrideRegistry::new();
        let mut last = TallyColor::Off;
        for color in colors {
            reg.set(&target, color);
            last = color;
        }
        prop_assert_eq!(reg.get(&target), Some(last));
        prop_assert!(reg.clear(&target), "a set override clears");
        prop_assert_eq!(reg.get(&target), None);
        // A second clear is a no-op (returns false).
        prop_assert!(!reg.clear(&target));
    }
}
