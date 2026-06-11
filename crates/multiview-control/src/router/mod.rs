//! External **router/switcher control** — SW-P-08 and Ember+ drivers, the route
//! table, and **route-follow** into the tally arbiter (broadcast-multiviewer
//! brief §2/§8).
//!
//! A broadcast router crosspoints sources to destinations. Multiview uses this for
//! two operator features:
//!
//! * **Name-following UMD** — when a destination's feed changes, the UMD label
//!   under the corresponding tile follows the *source's* name.
//! * **Route-follow tally** — a destination that is on a tally bus (program /
//!   preview) lights the tile that is currently routed to it.
//!
//! The two openly-published control protocols are the **pure codecs** in
//! [`swp08`] and [`ember`]. This module adds the protocol-agnostic [`RouteTable`]
//! (latest-wins destination→source map, the control-plane mirror of the router's
//! crosspoint state) and the pure [`route_follow`] step that turns a router route
//! report plus a [`RouteBinding`] into a tally/UMD update the existing
//! [`TallyMirror`](crate::tally_state::TallyMirror) records — with **no engine
//! back-pressure** (invariant #10): the route mirror is control-plane state only,
//! fed lossily, and the lamp it computes is *submitted* to the engine arbiter
//! exactly like a manual override.
//!
//! The live TCP/serial socket to a router is behind the off-by-default `router`
//! feature (`transport`); the codecs and the route-follow logic above are
//! always compiled and tested.
use std::collections::HashMap;
use std::sync::Mutex;

use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use multiview_events::{TallyEvent, TallyTarget};
use serde::{Deserialize, Serialize};

pub mod ember;
pub mod swp08;

#[cfg(feature = "router")]
pub mod transport;

/// A single router crosspoint: `destination` is fed from `source` on a `level`.
///
/// Protocol-agnostic — both [`swp08::SwP08Message::Connected`] and an Ember+
/// parameter update normalise into this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterRoute {
    /// The signal level (e.g. video, an audio group).
    pub level: u16,
    /// The destination (output) whose feed this describes.
    pub destination: u16,
    /// The source (input) currently feeding it.
    pub source: u16,
}

impl RouterRoute {
    /// Build a route from a SW-P-08 `Connected`/`Connect` report, if it carries a
    /// source. Returns [`None`] for an `Interrogate` (no source).
    #[must_use]
    pub fn from_swp08(message: &swp08::SwP08Message) -> Option<Self> {
        match *message {
            swp08::SwP08Message::Connected {
                level,
                destination,
                source,
                ..
            }
            | swp08::SwP08Message::Connect {
                level,
                destination,
                source,
                ..
            } => Some(Self {
                level,
                destination,
                source,
            }),
            swp08::SwP08Message::Interrogate { .. } => None,
        }
    }
}

/// The control-plane mirror of the router's crosspoint state.
///
/// A latest-wins map keyed by `(level, destination)` → `source`, fed lossily
/// from router reports. Control-plane state only; never on the engine's data
/// plane (invariant #10). Missing an intermediate route is safe — the next
/// report for that destination carries the current source.
#[derive(Debug, Default)]
pub struct RouteTable {
    routes: Mutex<HashMap<(u16, u16), u16>>,
}

impl RouteTable {
    /// A fresh, empty route table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner map, recovering from a poisoned lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(u16, u16), u16>> {
        match self.routes.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Record a route (latest wins for the same `(level, destination)`).
    pub fn apply(&self, route: RouterRoute) {
        self.lock()
            .insert((route.level, route.destination), route.source);
    }

    /// The source currently feeding `destination` on `level`, if known.
    #[must_use]
    pub fn source_of(&self, level: u16, destination: u16) -> Option<u16> {
        self.lock().get(&(level, destination)).copied()
    }

    /// Number of distinct `(level, destination)` routes held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// A binding from a router destination to a Multiview tally target.
///
/// This is the config-as-code mapping that makes route-follow meaningful: when
/// the router reports a feed change on `(level, destination)`, the lamp/label
/// for `target` follows it. `program_sources` lists the source ids that are
/// "on-air" for this binding (lighting [`TallyColor::Red`]); `preview_sources`
/// lights [`TallyColor::Green`]. A source in neither set leaves the lamp off but
/// still updates the name-following label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteBinding {
    /// The router level this binding watches.
    pub level: u16,
    /// The router destination this binding watches.
    pub destination: u16,
    /// The Multiview tally target the route drives.
    pub target: TallyTarget,
    /// Source ids that light the program (red) lamp when routed to `destination`.
    #[serde(default)]
    pub program_sources: Vec<u16>,
    /// Source ids that light the preview (green) lamp when routed to `destination`.
    #[serde(default)]
    pub preview_sources: Vec<u16>,
}

impl RouteBinding {
    /// The tally lamp colour a `source` routed to this binding implies.
    #[must_use]
    pub fn color_for(&self, source: u16) -> TallyColor {
        if self.program_sources.contains(&source) {
            TallyColor::Red
        } else if self.preview_sources.contains(&source) {
            TallyColor::Green
        } else {
            TallyColor::Off
        }
    }
}

/// The result of one route-follow step: the tally update a route report implies
/// for a binding, ready to record in the [`TallyMirror`](crate::tally_state::TallyMirror)
/// and/or submit to the engine arbiter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFollowUpdate {
    /// The Multiview target whose lamp/label changed.
    pub target: TallyTarget,
    /// The source now feeding it (for the name-following label).
    pub source: u16,
    /// The resolved tally lamp colour for that source under this binding.
    pub color: TallyColor,
}

impl RouteFollowUpdate {
    /// The [`TallyEvent`] this update mirrors (so it can be applied to the
    /// control-plane [`TallyMirror`](crate::tally_state::TallyMirror) exactly like
    /// an engine-published tally observation).
    ///
    /// The originating bus is recorded from the lamp colour: program (red),
    /// preview (green), and an off/amber lamp default to the program bus so the
    /// state is total. Brightness is full — a route-follow lamp is not dimmed.
    #[must_use]
    pub fn to_tally_event(&self) -> TallyEvent {
        let source = match self.color {
            TallyColor::Green => BusSource::Preview,
            _ => BusSource::Program,
        };
        TallyEvent {
            target: self.target.clone(),
            state: TallyState {
                color: self.color,
                brightness: Brightness::FULL,
                source,
            },
        }
    }
}

/// **Route-follow** (pure): given a router route report and the binding that
/// watches it, compute the tally/UMD update — or [`None`] if the report is for a
/// destination/level the binding does not watch.
///
/// This is the unit of behaviour the route-follow drive loop is built on: pure,
/// total, and exhaustively testable with no sockets. The caller records the
/// update in the control-plane mirror and submits the lamp to the engine arbiter
/// (never blocking the engine — invariant #10).
#[must_use]
pub fn route_follow(route: RouterRoute, binding: &RouteBinding) -> Option<RouteFollowUpdate> {
    if route.level != binding.level || route.destination != binding.destination {
        return None;
    }
    Some(RouteFollowUpdate {
        target: binding.target.clone(),
        source: route.source,
        color: binding.color_for(route.source),
    })
}

/// Apply a router route to every binding that watches it, returning the
/// route-follow updates produced (one per matching binding).
///
/// Pure and side-effect-free: the caller decides what to do with the updates
/// (mirror + submit). Multiple bindings may watch one destination (e.g. a tile
/// and a video-wall element), so this returns a vector.
#[must_use]
pub fn route_follow_all(route: RouterRoute, bindings: &[RouteBinding]) -> Vec<RouteFollowUpdate> {
    bindings
        .iter()
        .filter_map(|b| route_follow(route, b))
        .collect()
}

/// Ingest one router route report into the control-plane state: record the
/// crosspoint in the [`RouteTable`], run route-follow against the bindings, and
/// mirror each resulting tally lamp into the [`TallyMirror`](crate::tally_state::TallyMirror).
///
/// This is the **route-follow → tally arbiter** wiring (Wave C): a router feed
/// change updates Multiview's resolved tally exactly like an engine-published tally
/// observation, so the existing `GET /api/v1/tally` surface reflects it. Returns
/// the updates applied (for the caller to also submit to the engine arbiter as
/// `SetTallyOverride` commands if it drives the engine directly).
///
/// **Isolation (invariant #10):** every touched structure is control-plane state
/// only — the route table, the bindings, and the tally mirror. Nothing here
/// awaits the engine or back-pressures it; the route report is *sampled*, never
/// paces anything.
pub fn ingest_route(
    route: RouterRoute,
    bindings: &[RouteBinding],
    table: &RouteTable,
    mirror: &crate::tally_state::TallyMirror,
) -> Vec<RouteFollowUpdate> {
    table.apply(route);
    let updates = route_follow_all(route, bindings);
    for update in &updates {
        mirror.apply(update.to_tally_event());
    }
    updates
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::tally::TallyColor;
    use multiview_events::TallyTarget;

    use super::{
        ingest_route, route_follow, route_follow_all, swp08::SwP08Message, RouteBinding,
        RouteTable, RouterRoute,
    };
    use crate::tally_state::TallyMirror;

    fn tile(index: u32) -> TallyTarget {
        TallyTarget::Tile { index }
    }

    fn binding() -> RouteBinding {
        RouteBinding {
            level: 1,
            destination: 5,
            target: tile(3),
            program_sources: vec![10, 11],
            preview_sources: vec![20],
        }
    }

    #[test]
    fn route_from_swp08_connected_carries_source() {
        let msg = SwP08Message::Connected {
            matrix: 0,
            level: 1,
            destination: 5,
            source: 10,
        };
        let route = RouterRoute::from_swp08(&msg).unwrap();
        assert_eq!(route.level, 1);
        assert_eq!(route.destination, 5);
        assert_eq!(route.source, 10);
    }

    #[test]
    fn route_from_swp08_interrogate_has_no_source() {
        let msg = SwP08Message::Interrogate {
            matrix: 0,
            level: 1,
            destination: 5,
        };
        assert!(RouterRoute::from_swp08(&msg).is_none());
    }

    #[test]
    fn route_table_keeps_latest_source_per_destination() {
        let table = RouteTable::new();
        assert!(table.is_empty());
        table.apply(RouterRoute {
            level: 1,
            destination: 5,
            source: 10,
        });
        table.apply(RouterRoute {
            level: 1,
            destination: 5,
            source: 11,
        });
        // Different level is a different key.
        table.apply(RouterRoute {
            level: 2,
            destination: 5,
            source: 99,
        });
        assert_eq!(table.source_of(1, 5), Some(11));
        assert_eq!(table.source_of(2, 5), Some(99));
        assert_eq!(table.source_of(1, 6), None);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn binding_color_classifies_program_preview_and_off() {
        let b = binding();
        assert_eq!(b.color_for(10), TallyColor::Red);
        assert_eq!(b.color_for(11), TallyColor::Red);
        assert_eq!(b.color_for(20), TallyColor::Green);
        assert_eq!(b.color_for(99), TallyColor::Off);
    }

    #[test]
    fn route_follow_lights_program_lamp_for_a_program_source() {
        let route = RouterRoute {
            level: 1,
            destination: 5,
            source: 10,
        };
        let update = route_follow(route, &binding()).unwrap();
        assert_eq!(update.target, tile(3));
        assert_eq!(update.source, 10);
        assert_eq!(update.color, TallyColor::Red);
        // The mirror event carries the same lamp.
        assert_eq!(update.to_tally_event().state.color, TallyColor::Red);
    }

    #[test]
    fn route_follow_ignores_a_route_for_a_different_destination() {
        let route = RouterRoute {
            level: 1,
            destination: 6,
            source: 10,
        };
        assert!(route_follow(route, &binding()).is_none());
    }

    #[test]
    fn route_follow_ignores_a_route_on_a_different_level() {
        let route = RouterRoute {
            level: 2,
            destination: 5,
            source: 10,
        };
        assert!(route_follow(route, &binding()).is_none());
    }

    #[test]
    fn route_follow_all_fans_out_to_every_matching_binding() {
        let other = RouteBinding {
            level: 1,
            destination: 5,
            target: tile(99),
            program_sources: vec![10],
            preview_sources: vec![],
        };
        let unrelated = RouteBinding {
            level: 1,
            destination: 7,
            target: tile(7),
            program_sources: vec![10],
            preview_sources: vec![],
        };
        let bindings = vec![binding(), other, unrelated];
        let route = RouterRoute {
            level: 1,
            destination: 5,
            source: 10,
        };
        let updates = route_follow_all(route, &bindings);
        assert_eq!(updates.len(), 2);
        assert!(updates.iter().all(|u| u.color == TallyColor::Red));
        let targets: Vec<&TallyTarget> = updates.iter().map(|u| &u.target).collect();
        assert!(targets.contains(&&tile(3)));
        assert!(targets.contains(&&tile(99)));
    }

    #[test]
    fn binding_serialises_round_trip() {
        let b = binding();
        let json = serde_json::to_value(&b).unwrap();
        let back: RouteBinding = serde_json::from_value(json).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn ingest_route_records_the_crosspoint_and_mirrors_tally() {
        // The route-follow → tally arbiter wiring: a router feed change updates
        // both the control-plane route table and the resolved-tally mirror that
        // backs `GET /api/v1/tally`.
        let table = RouteTable::new();
        let mirror = TallyMirror::new();
        let bindings = vec![binding()];

        // Program source 10 routed to destination 5 → tile 3 lights red.
        let updates = ingest_route(
            RouterRoute {
                level: 1,
                destination: 5,
                source: 10,
            },
            &bindings,
            &table,
            &mirror,
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(table.source_of(1, 5), Some(10));
        let entry = mirror.get(&tile(3)).expect("tile 3 mirrored");
        assert_eq!(entry.state.color, TallyColor::Red);

        // Re-route to preview source 20 → tile 3 follows to green.
        ingest_route(
            RouterRoute {
                level: 1,
                destination: 5,
                source: 20,
            },
            &bindings,
            &table,
            &mirror,
        );
        assert_eq!(table.source_of(1, 5), Some(20));
        assert_eq!(mirror.get(&tile(3)).unwrap().state.color, TallyColor::Green);
    }

    #[test]
    fn ingest_route_for_an_unwatched_destination_mirrors_nothing() {
        let table = RouteTable::new();
        let mirror = TallyMirror::new();
        let updates = ingest_route(
            RouterRoute {
                level: 1,
                destination: 99,
                source: 10,
            },
            &[binding()],
            &table,
            &mirror,
        );
        assert!(updates.is_empty());
        // The route is still recorded even if no binding watches it.
        assert_eq!(table.source_of(1, 99), Some(10));
        assert!(mirror.is_empty());
    }
}
