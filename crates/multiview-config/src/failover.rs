//! The configurable **failover slate** policy (ADR-0027 / ADR-0030).
//!
//! When a source is lost — or "being a dick" (delivering nothing usable, or
//! garbage) — the engine is **bulletproof**: it never stalls, and it shows a
//! deliberate failover picture until the source recovers. This module carries
//! the single shared **policy** for *what* it shows, selectable identically in
//! two places:
//!
//! 1. **Per layout cell / source** ([`crate::Cell::on_loss`]) — the tile's slate
//!    when its source rides the tile state machine past the `STALE` → `NO_SIGNAL`
//!    ladder into the down state.
//! 2. **Per program** ([`crate::ProgramSpec::on_loss`]) — the program-level
//!    slate the non-layout **passthrough / transcode** case shows on input loss
//!    (the pre-baked GP-4 slate of ADR-0030 §4).
//!
//! Both surfaces use the **same** [`FailoverSlate`] type, so the operator
//! configures a tile and a passthrough program "the same way", and the two can
//! never drift apart.
//!
//! The policy chooses *what* shows on loss; the tile state machine (`LIVE` →
//! `STALE` → `RECONNECTING` → `NO_SIGNAL`, with the broadcast defaults hold
//! 500 ms / stale 2 s / no-signal 10 s) decides *when* the slate replaces the
//! held last-good frame — this policy does not change those thresholds (see
//! `docs/research/resilience-and-av.md` §1.3).

use serde::{Deserialize, Serialize};

/// What a tile or program shows when its source is lost or misbehaving — the
/// shared failover-slate policy (ADR-0027 / ADR-0030).
///
/// Internally tagged by `slate` (`#[serde(tag = "slate")]`) — robust across the
/// non-self-describing TOML and JSON wire forms, never `untagged` (ADR-0010 /
/// conventions §5). `#[non_exhaustive]` so future failover pictures (e.g. a
/// custom card or a clock) extend it without a breaking change; downstream
/// `match` carries a wildcard arm.
///
/// The default is [`FailoverSlate::Bars`] — the broadcast "we have a problem"
/// standard line-up signal (SMPTE/EBU colour bars, with the companion 1 kHz tone
/// where audio flows) — so a document that omits `on_loss` (every pre-existing
/// config) gets the standard, never a surprise dead screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "slate", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FailoverSlate {
    /// SMPTE/EBU 75 % colour **bars** — the line-up signal (the synthetic `bars`
    /// source of ADR-0027). The audio companion is a **1 kHz tone** where the run
    /// carries audio (modelled by the policy; emitted by the audio path once it
    /// flows — it is *not* fabricated here).
    Bars,
    /// The **signal-lost** card (the engine's `NO_SIGNAL` slate) — a distinct
    /// "no signal" placeholder.
    NoSignal,
    /// A full-frame **black** raster (limited-range luma 16, neutral chroma).
    Black,
}

impl Default for FailoverSlate {
    /// The broadcast default: colour [`Bars`](FailoverSlate::Bars).
    fn default() -> Self {
        Self::Bars
    }
}

/// The serde default for an omitted `on_loss` field: [`FailoverSlate::Bars`].
///
/// A free function (not a closure) because `#[serde(default = "…")]` names a
/// path; shared by [`crate::Cell::on_loss`] and [`crate::ProgramSpec::on_loss`]
/// so both surfaces default identically (back-compat: existing documents that
/// carry no `on_loss` get `Bars`).
#[must_use]
pub fn default_failover_slate() -> FailoverSlate {
    FailoverSlate::Bars
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn default_is_bars() {
        assert_eq!(FailoverSlate::default(), FailoverSlate::Bars);
        assert_eq!(default_failover_slate(), FailoverSlate::Bars);
    }

    #[test]
    fn round_trips_internally_tagged_never_untagged() {
        for slate in [
            FailoverSlate::Bars,
            FailoverSlate::NoSignal,
            FailoverSlate::Black,
        ] {
            let json = serde_json::to_string(&slate).unwrap();
            // The discriminant is a sibling `slate` field, not an untagged shape.
            assert!(json.contains("\"slate\":"), "internally tagged: {json}");
            let back: FailoverSlate = serde_json::from_str(&json).unwrap();
            assert_eq!(back, slate);
        }
    }

    #[test]
    fn snake_case_tokens() {
        let no_signal: FailoverSlate = serde_json::from_str(r#"{"slate":"no_signal"}"#).unwrap();
        assert_eq!(no_signal, FailoverSlate::NoSignal);
        let bars: FailoverSlate = serde_json::from_str(r#"{"slate":"bars"}"#).unwrap();
        assert_eq!(bars, FailoverSlate::Bars);
        let black: FailoverSlate = serde_json::from_str(r#"{"slate":"black"}"#).unwrap();
        assert_eq!(black, FailoverSlate::Black);
    }
}
