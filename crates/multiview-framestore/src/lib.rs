//! # multiview-framestore
//!
//! Per-tile **last-good-frame** stores and the tile **failure-ladder state
//! machine** — Multiview invariant #2.
//!
//! Each input decodes independently into a lock-free single-slot
//! [`LatestSlot`] (overwrite / newest-wins); the compositor
//! samples it on every output tick and **never blocks**. A
//! [`TileStore`] wraps that slot with a timing policy that
//! rides the per-tile failure ladder
//! (`LIVE -> STALE -> RECONNECTING -> NO_SIGNAL`, and back to `LIVE` the instant
//! a fresh frame arrives), holding the last-good frame on starvation and
//! surfacing an explicit [`NoSignal`](tile::TileRead::NoSignal) indicator when
//! there is nothing usable to show.
//!
//! Design source: `docs/research/resilience-and-av.md` §1.2–§1.3,
//! `docs/research/streaming-gotchas.md` §1 & §7, and ADR-T002.
//!
//! ## Invariants upheld here
//!
//! * **Reader never blocks / never tears** — reads clone an [`Arc`](std::sync::Arc)
//!   out of an [`arc_swap::ArcSwapOption`]; no locks, no `unsafe`.
//! * **Bounded memory** — single slot, newest wins; a fast source overwrites,
//!   a slow source is held.
//! * **Deterministic timing** — "now" is *injected* as a
//!   [`MediaTime`](multiview_core::time::MediaTime), so the whole ladder is
//!   property/state-machine testable with no real clock.
//!
//! ```
//! use multiview_framestore::{TileStore, TileThresholds, TileRead};
//! use multiview_core::time::MediaTime;
//!
//! let thresholds = TileThresholds::from_millis(500, 2_000, 10_000)
//!     .expect("hold < stale < nosignal");
//! let store: TileStore<u32> = TileStore::with_defaults("cam-1");
//! let _ = thresholds; // (use them with `TileStore::new` for custom timing)
//!
//! // No frame yet -> NoSignal.
//! assert!(matches!(store.read(MediaTime::ZERO), TileRead::NoSignal));
//!
//! // A frame arrives at t=0; reading at t=10ms (< hold) is Fresh.
//! store.publish(0xCAFE, MediaTime::ZERO);
//! let read = store.read(MediaTime::from_nanos(10_000_000));
//! assert!(matches!(read, TileRead::Fresh { .. }));
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod latest;
pub mod liveness;
pub mod state;
pub mod tile;

pub use error::{Error, Result};
pub use latest::LatestSlot;
pub use liveness::{PacketLiveness, PacketLivenessState, PacketLivenessThresholds};
pub use state::{classify, TileThresholds};
pub use tile::{NoSignalPolicy, TileRead, TileStore};
