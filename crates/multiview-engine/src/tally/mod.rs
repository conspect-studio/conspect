//! The M11 **tally** control surface: aggregate many external tally buses into a
//! resolved per-tile [`TallyState`](multiview_core::tally::TallyState)
//! (broadcast-multiviewer brief §2, ADR-MV001).
//!
//! Three pure layers, mirroring the alarm engine's structure:
//!
//! * [`profile`] — the configurable bit↔colour and index↔tile mapping that turns
//!   a bus's wire vocabulary into a Multiview tally state.
//! * [`gpio`] — a virtual GPI/GPO model (logical input/output points with
//!   polarity + edge/level semantics), the protocol-agnostic logic under physical
//!   GPI or NMOS IS-07.
//! * [`arbiter`] — aggregate the per-tick set of tally facts (PGM/PVW/ME/aux/
//!   router/GPI/IS-07) into one winning
//!   [`TallyState`](multiview_core::tally::TallyState) per tile under a defined
//!   conflict-resolution policy, with an optional anti-glitch latch.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Everything here is a pure value machine over an injected
//! [`MediaTime`](multiview_core::time::MediaTime). The arbiter and GPIO points
//! **return** resolved states; they never reach into the engine, never send on a
//! channel and never block. The engine samples them on its slow control tick and
//! renders the result at a frame boundary, so a stalled or absent tally tick can
//! never delay a frame or back-pressure the engine.
pub mod arbiter;
pub mod gpio;
pub mod profile;

pub use arbiter::{ConflictPolicy, LatchPolicy, TallyArbiter, TallyFact};
pub use gpio::{Edge, GpiPoint, GpoPoint, Polarity};
pub use profile::{BitMapping, TallyProfile};
